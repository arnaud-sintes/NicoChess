package com.nicochess.app

import android.content.Context

/** What the owner chose in the "This phone" card of the page. Kept between runs. */
object Settings {
    /** The page for this app, on the loopback address: nothing outside can reach it. */
    const val LOCAL_PORT = 8080

    /** The page for other devices, when the owner allows it. */
    const val REMOTE_PORT = 8081

    private const val FILE = "nicochess"

    private fun prefs(context: Context) = context.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    /** Other devices on the network may open the page. Off by default. */
    fun remote(context: Context) = prefs(context).getBoolean("remote", false)

    fun setRemote(context: Context, on: Boolean) = prefs(context).edit().putBoolean("remote", on).apply()

    /** Use the external (USB) camera instead of a camera of the phone. Off by default. */
    fun external(context: Context) = prefs(context).getBoolean("external", false)

    fun setExternal(context: Context, on: Boolean) = prefs(context).edit().putBoolean("external", on).apply()

    /** The camera of the phone to use ("" = the main back camera). */
    fun cameraId(context: Context): String = prefs(context).getString("camera", "") ?: ""

    fun setCameraId(context: Context, id: String) = prefs(context).edit().putString("camera", id).apply()
}
