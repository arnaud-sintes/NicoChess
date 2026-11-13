package com.nicochess.app

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.WindowManager
import android.webkit.JsResult
import android.webkit.URLUtil
import android.webkit.WebChromeClient
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Toast
import java.net.URL

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
            // Without a WebChromeClient every confirm() of the page answers "no": End game, Abort,
            // Forget calibration and Re-learn would do nothing.
            webChromeClient = object : WebChromeClient() {
                override fun onJsConfirm(view: WebView?, url: String?, message: String?, result: JsResult?): Boolean {
                    ask(message, result, withCancel = true)
                    return true
                }

                override fun onJsAlert(view: WebView?, url: String?, message: String?, result: JsResult?): Boolean {
                    ask(message, result, withCancel = false)
                    return true
                }
            }
            // The WebView ignores the "download" of a link: the PGN of a game is handed to the
            // share sheet instead (mail, notes, files...).
            setDownloadListener { url, _, disposition, mimeType, _ -> sharePgn(url, disposition, mimeType) }
            // The page of this app (and only it: the WebView never leaves the loopback address)
            // gets the controls of "This phone".
            addJavascriptInterface(AndroidBridge(this@MainActivity), "NicoAndroid")
        }
        setContentView(web)

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
                    web.loadUrl("http://127.0.0.1:${Settings.LOCAL_PORT}/")
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

    private fun ask(message: String?, result: JsResult?, withCancel: Boolean) {
        val dialog = AlertDialog.Builder(this)
            .setMessage(message ?: "")
            .setPositiveButton(android.R.string.ok) { _, _ -> result?.confirm() }
            .setOnCancelListener { result?.cancel() }
        if (withCancel) dialog.setNegativeButton(android.R.string.cancel) { _, _ -> result?.cancel() }
        dialog.show()
    }

    /** Reads a game from the local server (only from it) and offers it to the share sheet. */
    private fun sharePgn(url: String, disposition: String?, mimeType: String?) {
        if (!url.startsWith("http://127.0.0.1:${Settings.LOCAL_PORT}/")) return
        Thread {
            val text = try {
                URL(url).openStream().bufferedReader().use { it.readText() }
            } catch (e: Exception) {
                null
            }
            runOnUiThread {
                if (text == null) {
                    Toast.makeText(this, "The game could not be read", Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                val send = Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_SUBJECT, URLUtil.guessFileName(url, disposition, mimeType))
                    .putExtra(Intent.EXTRA_TEXT, text)
                startActivity(Intent.createChooser(send, "Share the game (PGN)"))
            }
        }.start()
    }

    override fun onResume() {
        super.onResume()
        // The camera permission may have been given since (in the settings of the phone): the service
        // then goes to the foreground, which keeps the camera alive with the screen off.
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            CoreService.start(this)
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
