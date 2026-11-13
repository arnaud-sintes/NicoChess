package com.nicochess.app

import android.graphics.ImageFormat
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.util.Log

/** One camera that Android offers to apps. */
data class CameraInfo(
    val id: String,
    /** "back", "front" or "external". */
    val facing: String,
    val label: String,
    val width: Int,
    val height: Int,
)

/** The cameras of the device, including a USB camera when the phone lists it. */
object CameraCatalog {
    private const val TAG = "nicochess"

    fun list(manager: CameraManager): List<CameraInfo> {
        val ids = try {
            manager.cameraIdList
        } catch (e: Exception) {
            Log.w(TAG, "cannot list the cameras: ${e.message}")
            return emptyList()
        }
        val found = ids.mapNotNull { id ->
            try {
                describe(manager, id)
            } catch (e: Exception) {
                Log.w(TAG, "camera $id skipped: ${e.message}")
                null
            }
        }
        // A phone can have several back cameras: the order of the ids puts the main one first.
        return found.sortedBy { rank(it) }
    }

    /** Back cameras first, then the front one, then external ones; the id keeps the order within a kind. */
    private fun rank(camera: CameraInfo) = when (camera.facing) {
        "back" -> 0
        "front" -> 1
        else -> 2
    }

    private fun describe(manager: CameraManager, id: String): CameraInfo? {
        val c = manager.getCameraCharacteristics(id)
        // Depth sensors, infrared cameras and the like cannot give an ordinary picture.
        val capabilities = c.get(CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES)
        if (capabilities != null && !capabilities.contains(CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES_BACKWARD_COMPATIBLE)) {
            return null
        }
        val sizes = c.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)?.getOutputSizes(ImageFormat.YUV_420_888)
        val largest = sizes?.maxByOrNull { it.width.toLong() * it.height } ?: return null

        val facing = when (c.get(CameraCharacteristics.LENS_FACING)) {
            CameraCharacteristics.LENS_FACING_BACK -> "back"
            CameraCharacteristics.LENS_FACING_FRONT -> "front"
            CameraCharacteristics.LENS_FACING_EXTERNAL -> "external"
            else -> "back"
        }
        val kind = when (facing) {
            "back" -> "Back camera" + lens(c)
            "front" -> "Front camera"
            else -> "External camera (USB)"
        }
        return CameraInfo(id, facing, "$kind, id $id", largest.width, largest.height)
    }

    /** " (ultra-wide)", " (telephoto)" or "": a guess from the focal length, to tell back cameras apart. */
    private fun lens(c: CameraCharacteristics): String {
        val focal = c.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS)?.firstOrNull() ?: return ""
        val sensorWidth = c.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE)?.width ?: return ""
        if (sensorWidth <= 0f) return ""
        val equivalent = focal * 36f / sensorWidth
        return when {
            equivalent < 20f -> " (ultra-wide)"
            equivalent > 60f -> " (telephoto)"
            else -> ""
        }
    }

    /** The camera the settings ask for, or why there is none. */
    fun choose(list: List<CameraInfo>, external: Boolean, wantedId: String): Choice {
        if (list.isEmpty()) return Choice(null, "This device has no camera")
        if (external) {
            val usb = list.firstOrNull { it.facing == "external" }
            return if (usb != null) {
                Choice(usb, null)
            } else {
                Choice(
                    null,
                    "The external camera is switched on but none is connected. Plug it in, or switch it off. " +
                        "Android only offers a USB camera to apps on phones that support it.",
                )
            }
        }
        val own = list.filter { it.facing != "external" }
        val pick = own.firstOrNull { it.id == wantedId } ?: own.firstOrNull() ?: list.first()
        return Choice(pick, null)
    }

    class Choice(val camera: CameraInfo?, val error: String?)
}
