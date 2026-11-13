package com.nicochess.app

import java.nio.ByteBuffer

/** The Rust core (libnicochess_android.so). */
object NativeCore {
    init {
        System.loadLibrary("nicochess_android")
    }

    /**
     * Starts the web server inside this process and waits until it accepts requests.
     * [source] is "phone" (the camera of the device), "demo" or "synthetic".
     * Returns "" when it is up (also when it already was), otherwise the reason it is not.
     */
    external fun start(dataDir: String, listen: String, source: String): String

    /**
     * Opens (or closes) the listener on the network, on [port], so that other devices on the
     * same network can open the page. The local one on 127.0.0.1 is always there.
     * Returns "" when done, otherwise the reason it did not work.
     */
    external fun setRemote(enabled: Boolean, port: Int): String

    /** True while a game is being recorded: the camera may not be changed then. */
    external fun isPlaying(): Boolean

    /** True while the core needs pictures: the camera should be open. */
    external fun cameraWanted(): Boolean

    /** Tells the core that the camera cannot be used; it shows the reason on the page. */
    external fun cameraFailed(message: String)

    /**
     * One picture, as the three planes of an Android YUV_420_888 image (direct buffers).
     * The buffers are only read during the call.
     */
    external fun pushFrame(
        width: Int,
        height: Int,
        y: ByteBuffer,
        yStride: Int,
        u: ByteBuffer,
        v: ByteBuffer,
        uvStride: Int,
        uvPixelStride: Int,
    )
}
