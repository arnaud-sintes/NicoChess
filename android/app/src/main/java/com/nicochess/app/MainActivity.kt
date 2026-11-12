package com.nicochess.app

import android.app.Activity
import android.os.Bundle
import android.view.WindowManager
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Shows the NicoChess page. The server runs in this same process, on the loopback
 * address, so nothing is reachable from the network.
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
                    if (web.url == null) web.loadUrl("http://127.0.0.1:$PORT/")
                } else {
                    showError(error)
                }
            }
        }.start()
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

        /** "demo" plays a game on a synthetic board. The cameras come next. */
        const val SOURCE = "demo"
    }
}
