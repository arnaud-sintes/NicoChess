package com.nicochess.app

import android.content.Context
import android.hardware.camera2.CameraManager
import android.webkit.JavascriptInterface
import org.json.JSONArray
import org.json.JSONObject

/**
 * What the page can ask of the app, as `window.NicoAndroid`. Only the WebView of this app
 * has it: the same page opened in a browser on another device does not, and hides the
 * "This phone" card.
 *
 * The calls arrive on a thread of the WebView, not the main thread.
 */
class AndroidBridge(context: Context) {
    private val app = context.applicationContext
    private val manager = app.getSystemService(Context.CAMERA_SERVICE) as CameraManager

    /** Everything the "This phone" card shows, as JSON. */
    @JavascriptInterface
    fun info(): String {
        val service = CoreService.instance
        val cameras = CameraCatalog.list(manager)
        val json = JSONObject()

        val remote = JSONObject()
        remote.put("enabled", Settings.remote(app))
        remote.put("port", Settings.REMOTE_PORT)
        remote.put("urls", JSONArray(CoreService.addresses()))
        remote.put("error", service?.remoteError ?: "")
        json.put("remote", remote)

        val external = JSONObject()
        external.put("enabled", Settings.external(app))
        external.put("present", cameras.any { it.facing == "external" })
        json.put("external", external)

        val list = JSONArray()
        for (camera in cameras) {
            list.put(
                JSONObject()
                    .put("id", camera.id)
                    .put("facing", camera.facing)
                    .put("label", camera.label)
                    .put("width", camera.width)
                    .put("height", camera.height),
            )
        }
        val chosen = CameraCatalog.choose(cameras, false, Settings.cameraId(app)).camera
        val camera = JSONObject()
        camera.put("cameras", list)
        camera.put("selected", chosen?.id ?: "")
        camera.put("using", service?.cameraDescription ?: "")
        json.put("camera", camera)

        json.put("error", service?.startError ?: "")
        return json.toString()
    }

    /** Lets other devices on the network open the page (or not). */
    @JavascriptInterface
    fun setRemote(on: Boolean) {
        Settings.setRemote(app, on)
        CoreService.instance?.settingsChanged()
    }

    /** Uses the external (USB) camera instead of one of the phone (or not). */
    @JavascriptInterface
    fun setExternal(on: Boolean) {
        Settings.setExternal(app, on)
        CoreService.instance?.settingsChanged()
    }

    /** Chooses which camera of the phone to use, by its id. */
    @JavascriptInterface
    fun setCamera(id: String) {
        Settings.setCameraId(app, id)
        CoreService.instance?.settingsChanged()
    }
}
