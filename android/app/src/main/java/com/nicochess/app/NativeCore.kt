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
