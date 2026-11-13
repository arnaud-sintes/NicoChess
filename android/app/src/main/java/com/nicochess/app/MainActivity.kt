package com.nicochess.app

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.WindowManager
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Shows the NicoChess page. The server runs in this same process, on the loopback
 * address, so nothing is reachable from the network.
 *
 * The camera of the phone is opened only while the page needs it (calibrating or
 * playing) and only while this screen is in front.
 */
class MainActivity : Activity() {
    private lateinit var web: WebView
    private lateinit var camera: PhoneCamera
    private val ui = Handler(Looper.getMainLooper())

    private var serverUp = false
    private var inFront = false
    private var permissionAsked = false

    private val poll = object : Runnable {
        override fun run() {
            syncCamera()
            ui.postDelayed(this, POLL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A game is followed on this screen: do not let it go dark.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        camera = PhoneCamera(this)
        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            // The page keeps its client token in localStorage.
            settings.domStorageEnabled = true
            webViewClient = WebViewClient()
        }
        setContentView(web)

        if (savedInstanceState != null) {
            web.restoreState(savedInstanceState)
        }
        Thread {
            val error = try {
                NativeCore.start(filesDir.absolutePath, "127.0.0.1:$PORT", SOURCE)
            } catch (e: Throwable) {
                e.toString()
            }
            runOnUiThread {
                if (error.isEmpty()) {
                    serverUp = true
                    if (web.url == null) web.loadUrl("http://127.0.0.1:$PORT/")
                } else {
                    showError(error)
                }
            }
        }.start()
    }

    override fun onResume() {
        super.onResume()
        inFront = true
        ui.post(poll)
    }

    override fun onPause() {
        inFront = false
        ui.removeCallbacks(poll)
        // The camera belongs to the screen in front. The core notices the silence and
        // asks again, so the picture comes back with the screen.
        camera.close()
        super.onPause()
    }

    override fun onDestroy() {
        ui.removeCallbacks(poll)
        camera.close()
        super.onDestroy()
    }

    /** Opens or closes the camera to match what the core wants. */
    private fun syncCamera() {
        if (!serverUp || !inFront) return
        if (!NativeCore.cameraWanted()) {
            camera.close()
            return
        }
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            camera.open()
        } else {
            if (!permissionAsked) {
                permissionAsked = true
                requestPermissions(arrayOf(Manifest.permission.CAMERA), REQUEST_CAMERA)
            }
            NativeCore.cameraFailed("Camera permission needed: allow it when asked, or in the settings of the app")
        }
    }

    private fun showError(message: String) {
        val escaped = message.replace("&", "&amp;").replace("<", "&lt;")
        web.loadDataWithBaseURL(
            null,
            "<body style=\"font-family:sans-serif;padding:16px\"><h2>NicoChess cannot start</h2><pre>$escaped</pre></body>",
            "text/html",
            "utf-8",
            null,
        )
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        web.saveState(outState)
    }

    @Deprecated("Back goes through the page history first.")
    override fun onBackPressed() {
        if (web.canGoBack()) web.goBack() else super.onBackPressed()
    }

    private companion object {
        const val PORT = 8080
        const val POLL_MS = 300L
        const val REQUEST_CAMERA = 1

        /** "phone": the camera of the device. ("demo" plays a game on a drawn board.) */
        const val SOURCE = "phone"
    }
}
