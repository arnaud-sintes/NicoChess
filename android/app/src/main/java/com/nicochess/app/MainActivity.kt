package com.nicochess.app

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.WindowManager
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Shows the NicoChess page. The server runs in this same process, on the loopback
 * address, so nothing is reachable from the network unless the owner switches that on
 * in the "This phone" card of the page.
 *
 * The server and the camera belong to [CoreService], so a game goes on being recorded
 * when this screen is left or the phone is locked.
 */
class MainActivity : Activity() {
    private lateinit var web: WebView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A game is followed on this screen: do not let it go dark.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            // The page keeps its client token in localStorage.
            settings.domStorageEnabled = true
            webViewClient = WebViewClient()
            // The page of this app (and only it: the WebView never leaves the loopback address)
            // gets the controls of "This phone".
            addJavascriptInterface(AndroidBridge(this@MainActivity), "NicoAndroid")
        }
        setContentView(web)

        if (savedInstanceState != null) {
            web.restoreState(savedInstanceState)
        }
        askForPermissions(savedInstanceState == null)

        // The service starts the core too; starting is safe to ask twice, and this call
        // returns when the server accepts requests, so the page can be loaded.
        Thread {
            val error = try {
                NativeCore.start(filesDir.absolutePath, "127.0.0.1:${Settings.LOCAL_PORT}", SOURCE)
            } catch (e: Throwable) {
                e.toString()
            }
            runOnUiThread {
                if (error.isEmpty()) {
                    if (web.url == null) web.loadUrl("http://127.0.0.1:${Settings.LOCAL_PORT}/")
                } else {
                    showError(error)
                }
            }
        }.start()
    }

    /**
     * The camera, and the notification of the service (Android 13 and later). Asked once,
     * when the app is opened; the service starts after the answer. Without the camera
     * permission the app still opens, and the page says the camera is not allowed.
     */
    private fun askForPermissions(first: Boolean) {
        val missing = mutableListOf<String>()
        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            missing += Manifest.permission.CAMERA
        }
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            missing += Manifest.permission.POST_NOTIFICATIONS
        }
        if (missing.isNotEmpty() && first) {
            requestPermissions(missing.toTypedArray(), REQUEST_PERMISSIONS)
        } else {
            CoreService.start(this)
        }
    }

    @Deprecated("Deprecated in the framework; this app targets the plain Activity API.")
    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        CoreService.start(this)
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
        const val REQUEST_PERMISSIONS = 1

        /** "phone": the camera of the device. ("demo" plays a game on a drawn board.) */
        const val SOURCE = "phone"
    }
}
