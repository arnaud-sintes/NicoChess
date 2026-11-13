package com.nicochess.app

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.Process
import android.util.Log
import java.net.Inet4Address
import java.net.NetworkInterface
import java.util.concurrent.Executors

/**
 * Keeps NicoChess alive while the phone is locked or another app is in front: it runs the
 * server, owns the camera and, when the owner allows it, the listener for other devices.
 * It shows a notification meanwhile (Android requires one for a service that uses the
 * camera), with a Stop button.
 *
 * The screen ([MainActivity]) only shows the page and the controls of "This phone".
 */
class CoreService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var camera: PhoneCamera

    @Volatile
    private var serverUp = false

    private var wakeLock: PowerManager.WakeLock? = null

    /** Why the server for other devices could not be opened, or "". */
    @Volatile
    var remoteError = ""
        private set

    /** Why the core could not start, or "". */
    @Volatile
    var startError = ""
        private set

    /** The camera in use and its size, or "". */
    val cameraDescription: String get() = camera.description

    private val poll = object : Runnable {
        override fun run() {
            syncCamera()
            main.postDelayed(this, POLL_MS)
        }
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        camera = PhoneCamera(this)
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopEverything()
            return START_NOT_STICKY
        }
        goForeground()
        startCore()
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        main.removeCallbacks(poll)
        camera.close()
        syncWakeLock()
        worker.shutdown()
        instance = null
        super.onDestroy()
    }

    // ---- The core -------------------------------------------------------------------

    private fun startCore() {
        if (serverUp) return
        worker.execute {
            val error = try {
                NativeCore.start(filesDir.absolutePath, "127.0.0.1:${Settings.LOCAL_PORT}", SOURCE)
            } catch (e: Throwable) {
                e.toString()
            }
            main.post {
                startError = error
                if (error.isEmpty()) {
                    serverUp = true
                    main.removeCallbacks(poll)
                    main.post(poll)
                }
            }
            if (error.isEmpty()) applyRemote()
        }
    }

    /** Opens or closes the server for other devices to match the settings. Slow: not on the main thread. */
    private fun applyRemote() {
        worker.execute {
            val error = try {
                NativeCore.setRemote(Settings.remote(this), Settings.REMOTE_PORT)
            } catch (e: Throwable) {
                e.toString()
            }
            remoteError = error
            main.post { refreshNotification() }
        }
    }

    /**
     * The owner changed a setting: take it into account now. Any thread. Only a change of camera
     * restarts the camera (it is opened again with the new choice by the next poll): the picture
     * must not be disturbed by the other settings, in the middle of a game.
     */
    fun settingsChanged(cameraAffected: Boolean) {
        main.post {
            if (cameraAffected) camera.close()
            if (serverUp) applyRemote()
            syncWakeLock()
        }
    }

    /** Opens or closes the camera to match what the core wants. */
    private fun syncCamera() {
        if (!serverUp) return
        if (!NativeCore.cameraWanted()) {
            camera.close()
        } else if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            camera.open()
        } else {
            NativeCore.cameraFailed("Camera permission needed: allow it in the settings of the app")
        }
        syncWakeLock()
    }

    /**
     * With the screen off the processor may go to sleep and the pictures stop: keep it awake
     * for as long as the camera is open, and not longer.
     */
    private fun syncWakeLock() {
        val lock = wakeLock ?: (getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "nicochess:camera")
            .also { it.setReferenceCounted(false); wakeLock = it })
        if (camera.active && !lock.isHeld) lock.acquire()
        if (!camera.active && lock.isHeld) lock.release()
    }

    // ---- Notification -----------------------------------------------------------------

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= 26) {
            val channel = NotificationChannel(CHANNEL, "NicoChess running", NotificationManager.IMPORTANCE_LOW)
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    private fun notification(): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, CoreService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE,
        )
        val text = if (Settings.remote(this) && remoteError.isEmpty()) {
            "Other devices: " + (addresses().firstOrNull() ?: "no network")
        } else {
            "Recording is possible while the screen is off"
        }
        @Suppress("DEPRECATION")
        val builder = if (Build.VERSION.SDK_INT >= 26) Notification.Builder(this, CHANNEL) else Notification.Builder(this)
        return builder
            .setSmallIcon(android.R.drawable.ic_menu_camera)
            .setContentTitle("NicoChess is running")
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, android.R.drawable.ic_menu_close_clear_cancel), "Stop", stop,
                ).build(),
            )
            .build()
    }

    private fun refreshNotification() {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification())
    }

    /**
     * Goes to the foreground, which keeps the camera usable with the screen off. A service of the
     * camera type is refused without the camera permission (Android 11 and later), so then it
     * stays an ordinary service: it works while the screen is on.
     */
    private fun goForeground() {
        val cameraAllowed = checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
        if (Build.VERSION.SDK_INT >= 30 && !cameraAllowed) {
            Log.w(TAG, "no camera permission: running as an ordinary service")
            return
        }
        try {
            val notification = notification()
            if (Build.VERSION.SDK_INT >= 30) {
                startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA)
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Exception) {
            Log.w(TAG, "cannot run in the foreground: ${e.message}")
        }
    }

    /** The Stop button: everything ends, including the server threads of the core. */
    private fun stopEverything() {
        camera.close()
        stopForeground(true)
        stopSelf()
        main.postDelayed({ Process.killProcess(Process.myPid()) }, 300)
    }

    companion object {
        private const val TAG = "nicochess"
        private const val CHANNEL = "nicochess"
        private const val NOTIFICATION_ID = 1
        private const val POLL_MS = 300L
        const val ACTION_STOP = "com.nicochess.app.STOP"

        /** "phone": the camera of the device. */
        private const val SOURCE = "phone"

        @Volatile
        var instance: CoreService? = null
            private set

        fun start(context: Context) {
            val intent = Intent(context, CoreService::class.java)
            // startForegroundService obliges the service to call startForeground within seconds,
            // which it only does with the camera permission (see goForeground).
            val foreground = Build.VERSION.SDK_INT >= 26 &&
                context.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
            try {
                if (foreground) context.startForegroundService(intent) else context.startService(intent)
            } catch (e: Exception) {
                Log.w(TAG, "cannot start the service: ${e.message}")
            }
        }

        /** Addresses at which other devices on the network can open the page. */
        fun addresses(): List<String> {
            val found = mutableListOf<String>()
            try {
                for (network in NetworkInterface.getNetworkInterfaces()) {
                    if (!network.isUp || network.isLoopback || network.isPointToPoint) continue
                    for (address in network.inetAddresses) {
                        if (address is Inet4Address && address.isSiteLocalAddress) {
                            found += "http://${address.hostAddress}:${Settings.REMOTE_PORT}/"
                        }
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "cannot list the addresses: ${e.message}")
            }
            return found
        }
    }
}
