package com.nicochess.app

import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.util.Size
import kotlin.math.abs

/**
 * A camera of the phone (or tablet), or an external USB camera when Android lists it,
 * through the Camera2 API. Which one is decided by [Settings].
 *
 * It captures YUV pictures and hands a good many per second to the Rust core. It is opened
 * and closed on request: the core says through [NativeCore.cameraWanted] when it needs
 * pictures. After a short moment the exposure, white balance and focus are locked,
 * because a picture that keeps changing upsets the detection of moves.
 *
 * Call [open] and [close] from the main thread. The camera callbacks run on a thread of
 * their own and only ask the main thread to act.
 */
class PhoneCamera(private val context: Context) {
    private val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
    private val main = Handler(Looper.getMainLooper())
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null

    /** Bumped at each open and close, so callbacks of an old camera can tell they are late. */
    @Volatile
    private var generation = 0

    @Volatile
    private var lastPushed = 0L

    /** True from [open] until [close], also while the camera is still starting. */
    @Volatile
    var active = false
        private set

    /** Which camera is open, and at which size: for the page to show. Empty when none. */
    @Volatile
    var description = ""
        private set

    fun open() {
        if (active) return
        active = true
        val mine = ++generation
        description = ""
        try {
            val choice = CameraCatalog.choose(
                CameraCatalog.list(manager),
                Settings.external(context),
                Settings.cameraId(context),
            )
            val chosen = choice.camera
            if (chosen == null) {
                failNow(choice.error ?: "No camera")
                return
            }
            val characteristics = manager.getCameraCharacteristics(chosen.id)
            val size = chooseSize(characteristics)
            description = "${chosen.label}, ${size.width}x${size.height}"
            val worker = HandlerThread("nicochess-camera").also { it.start() }
            val workerHandler = Handler(worker.looper)
            thread = worker
            handler = workerHandler

            val imageReader = ImageReader.newInstance(size.width, size.height, ImageFormat.YUV_420_888, 2)
            imageReader.setOnImageAvailableListener({ source -> onImage(source, mine) }, workerHandler)
            reader = imageReader
            Log.i(TAG, "opening $description")

            manager.openCamera(chosen.id, object : CameraDevice.StateCallback() {
                override fun onOpened(camera: CameraDevice) {
                    if (mine != generation) {
                        camera.close()
                        return
                    }
                    device = camera
                    startSession(camera, imageReader, characteristics, mine)
                }

                override fun onDisconnected(camera: CameraDevice) {
                    camera.close()
                    failLater(mine, "The camera was disconnected")
                }

                override fun onError(camera: CameraDevice, error: Int) {
                    camera.close()
                    failLater(mine, "The camera failed (error $error)")
                }
            }, workerHandler)
        } catch (e: SecurityException) {
            failNow("Camera permission refused: allow it in the settings of the app")
        } catch (e: CameraAccessException) {
            failNow("Cannot open the camera: ${e.message}")
        } catch (e: RuntimeException) {
            failNow("Cannot open the camera: ${e.message}")
        }
    }

    fun close() {
        if (!active) return
        active = false
        generation++
        description = ""
        try {
            session?.stopRepeating()
        } catch (_: Exception) {
        }
        session?.close()
        device?.close()
        reader?.close()
        thread?.quitSafely()
        session = null
        device = null
        reader = null
        thread = null
        handler = null
        Log.i(TAG, "camera closed")
    }

    /** From the main thread: gives the camera up and tells the core why. The core asks again soon. */
    private fun failNow(message: String) {
        Log.w(TAG, message)
        close()
        NativeCore.cameraFailed(message)
    }

    /** From a camera thread: the main thread does it, unless the camera has been closed since. */
    private fun failLater(mine: Int, message: String) {
        main.post { if (mine == generation && active) failNow(message) }
    }

    /** About 1280x720: sharp enough for the pieces, light enough to convert a few times a second. */
    private fun chooseSize(characteristics: CameraCharacteristics): Size {
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
        val sizes = map?.getOutputSizes(ImageFormat.YUV_420_888)
            ?: throw IllegalStateException("the camera offers no YUV pictures")
        val target = TARGET_WIDTH * TARGET_HEIGHT
        return sizes.filter { it.width >= 640 && it.height >= 480 }
            .minByOrNull { abs(it.width * it.height - target) }
            ?: sizes.maxBy { it.width * it.height }
    }

    @Suppress("DEPRECATION")
    private fun startSession(
        camera: CameraDevice,
        imageReader: ImageReader,
        characteristics: CameraCharacteristics,
        mine: Int,
    ) {
        try {
            camera.createCaptureSession(
                listOf(imageReader.surface),
                object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(configured: CameraCaptureSession) {
                        if (mine != generation) {
                            configured.close()
                            return
                        }
                        session = configured
                        startRepeating(camera, configured, imageReader, characteristics, mine)
                    }

                    override fun onConfigureFailed(failed: CameraCaptureSession) {
                        failLater(mine, "The camera could not be configured")
                    }
                },
                handler,
            )
        } catch (e: Exception) {
            failLater(mine, "Cannot start the camera: ${e.message}")
        }
    }

    private fun startRepeating(
        camera: CameraDevice,
        session: CameraCaptureSession,
        imageReader: ImageReader,
        characteristics: CameraCharacteristics,
        mine: Int,
    ) {
        try {
            val builder = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW)
            builder.addTarget(imageReader.surface)
            // An external camera may have a fixed focus: only ask for what it offers.
            if (offers(characteristics, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE)) {
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE)
            }
            session.setRepeatingRequest(builder.build(), null, handler)

            // Let exposure, white balance and focus settle on the board, then freeze them.
            handler?.postDelayed({
                if (mine == generation) lock(camera, session, imageReader, characteristics)
            }, SETTLE_MS)
        } catch (e: Exception) {
            failLater(mine, "Cannot start the camera: ${e.message}")
        }
    }

    private fun offers(characteristics: CameraCharacteristics, afMode: Int): Boolean =
        characteristics.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)?.contains(afMode) == true

    /**
     * Best effort: a device that cannot lock a setting keeps it automatic. Switching
     * the focus from continuous to plain auto without asking for a new scan keeps the
     * lens where it is.
     */
    private fun lock(
        camera: CameraDevice,
        session: CameraCaptureSession,
        imageReader: ImageReader,
        characteristics: CameraCharacteristics,
    ) {
        try {
            val builder = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW)
            builder.addTarget(imageReader.surface)
            val applied = mutableListOf<String>()
            if (characteristics.get(CameraCharacteristics.CONTROL_AE_LOCK_AVAILABLE) == true) {
                builder.set(CaptureRequest.CONTROL_AE_LOCK, true)
                applied += "exposure"
            }
            if (characteristics.get(CameraCharacteristics.CONTROL_AWB_LOCK_AVAILABLE) == true) {
                builder.set(CaptureRequest.CONTROL_AWB_LOCK, true)
                applied += "white balance"
            }
            if (offers(characteristics, CaptureRequest.CONTROL_AF_MODE_AUTO)) {
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_AUTO)
                applied += "focus"
            }
            session.setRepeatingRequest(builder.build(), null, handler)
            Log.i(TAG, "locked: ${applied.joinToString()}")
        } catch (e: Exception) {
            Log.w(TAG, "could not lock the camera settings: ${e.message}")
        }
    }

    private fun onImage(source: ImageReader, mine: Int) {
        val image = try {
            source.acquireLatestImage()
        } catch (_: Exception) {
            null
        } ?: return
        try {
            val now = SystemClock.elapsedRealtime()
            if (mine != generation || now - lastPushed < PUSH_INTERVAL_MS) return
            lastPushed = now
            val planes = image.planes
            NativeCore.pushFrame(
                image.width,
                image.height,
                planes[0].buffer,
                planes[0].rowStride,
                planes[1].buffer,
                planes[2].buffer,
                planes[1].rowStride,
                planes[1].pixelStride,
            )
        } finally {
            image.close()
        }
    }

    private companion object {
        const val TAG = "nicochess"
        const val TARGET_WIDTH = 1280
        const val TARGET_HEIGHT = 720
        const val SETTLE_MS = 1500L

        /**
         * Pictures per second handed to the core: about 16, so the preview is smooth. The
         * detection of moves looks at fewer of them (see track_interval in the native library).
         */
        const val PUSH_INTERVAL_MS = 62L
    }
}
