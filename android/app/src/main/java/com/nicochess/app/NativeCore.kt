package com.nicochess.app

/** The Rust core (libnicochess_android.so). */
object NativeCore {
    init {
        System.loadLibrary("nicochess_android")
    }

    /**
     * Starts the web server inside this process and waits until it accepts requests.
     * Returns "" when it is up (also when it already was), otherwise the reason it is not.
     */
    external fun start(dataDir: String, listen: String, source: String): String
}
