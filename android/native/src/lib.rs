//! The Android side of NicoChess: the core library behind a JNI entry point.
//!
//! The app (Kotlin) calls `NativeCore.start(...)` once. That starts the web
//! server in a thread of this process, on the loopback address, and the app
//! shows the page in a WebView.

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JString};
use jni::sys::{jboolean, jint, jstring};
use nicochess::app::{self, ServerOptions};
use nicochess::camera::SourceSpec;
use nicochess::pushed::{FrameInbox, yuv420_to_rgb};
use nicochess::tracker::TrackerConfig;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

static STARTED: AtomicBool = AtomicBool::new(false);

/// Where the app drops the pictures of the phone's camera.
fn inbox() -> Arc<FrameInbox> {
    static INBOX: OnceLock<Arc<FrameInbox>> = OnceLock::new();
    INBOX.get_or_init(FrameInbox::new).clone()
}

/// `String NativeCore.start(String dataDir, String listen, String source)`
///
/// Returns an empty string when the server is up (or already was), otherwise the
/// reason it could not start. Blocks until the server accepts requests.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_start<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    data_dir: JString<'l>,
    listen: JString<'l>,
    source: JString<'l>,
) -> jstring {
    let mut read = |s: &JString<'l>| -> Result<String, String> {
        env.get_string(s).map(String::from).map_err(|e| e.to_string())
    };
    let result = match (read(&data_dir), read(&listen), read(&source)) {
        (Ok(d), Ok(l), Ok(s)) => std::panic::catch_unwind(|| start(d, l, s))
            .unwrap_or_else(|_| Err("the native code panicked".to_string())),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
    };
    let message = result.err().unwrap_or_default();
    env.new_string(message).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// `boolean NativeCore.cameraWanted()`: true while the program needs pictures,
/// so the app should have its camera open.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_cameraWanted<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jboolean {
    jboolean::from(inbox().wanted())
}

/// `void NativeCore.cameraFailed(String message)`: the app could not open its camera.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_cameraFailed<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    message: JString<'l>,
) {
    let text = env.get_string(&message).map(String::from).unwrap_or_else(|_| "the camera failed".to_string());
    inbox().fail(text);
}

/// `void NativeCore.pushFrame(...)`: one picture of the camera, as Android's
/// YUV_420_888 planes (direct buffers). The buffers are only read during the call.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_pushFrame<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    width: jint,
    height: jint,
    y: JByteBuffer<'l>,
    y_stride: jint,
    u: JByteBuffer<'l>,
    v: JByteBuffer<'l>,
    uv_stride: jint,
    uv_pixel_stride: jint,
) {
    let plane = |buffer: &JByteBuffer<'l>| -> Option<&[u8]> {
        let address = env.get_direct_buffer_address(buffer).ok()?;
        let length = env.get_direct_buffer_capacity(buffer).ok()?;
        if address.is_null() {
            return None;
        }
        // SAFETY: the app passes direct buffers of a live Image and does not close it
        // before this call returns; the slice does not outlive the call.
        Some(unsafe { std::slice::from_raw_parts(address, length) })
    };
    let (Some(y), Some(u), Some(v)) = (plane(&y), plane(&u), plane(&v)) else {
        inbox().fail("the camera gave pictures that cannot be read (no direct buffers)");
        return;
    };
    let dimensions = (u32::try_from(width), u32::try_from(height));
    let strides = (usize::try_from(y_stride), usize::try_from(uv_stride), usize::try_from(uv_pixel_stride));
    let (Ok(width), Ok(height), Ok(y_stride), Ok(uv_stride), Ok(uv_pixel_stride)) =
        (dimensions.0, dimensions.1, strides.0, strides.1, strides.2)
    else {
        inbox().fail("the camera gave a picture with a negative size or stride");
        return;
    };
    match yuv420_to_rgb(width, height, y, y_stride, u, v, uv_stride, uv_pixel_stride) {
        Ok(image) => inbox().push(image),
        Err(e) => inbox().fail(format!("{e:#}")),
    }
}

fn start(data_dir: String, listen: String, source: String) -> Result<(), String> {
    if STARTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    redirect_output_to_logcat();

    let spec = match source.as_str() {
        "phone" => SourceSpec::Pushed(inbox()),
        "demo" => SourceSpec::Demo { move_interval: Duration::from_secs(4) },
        "synthetic" => SourceSpec::Synthetic,
        other => {
            STARTED.store(false, Ordering::SeqCst);
            return Err(format!("unknown source \"{other}\" (this build has: phone, demo, synthetic)"));
        }
    };

    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let ready_tx = tx.clone();
    let spawned = std::thread::Builder::new().name("nicochess-server".into()).spawn(move || {
        let dir = PathBuf::from(data_dir);
        let options = ServerOptions {
            listen,
            source: spec,
            frame_interval: Duration::from_millis(250),
            data_dir: dir.join("games"),
            calibration_path: dir.join("calibration.json"),
            tracker: TrackerConfig::default(),
            // The preview is fed faster than the detection needs.
            track_interval: Duration::from_millis(200),
            idle_timeout: Duration::from_secs(60),
            game_timeout: Duration::from_secs(30 * 60),
            max_viewers: 8,
        };
        let ready: Box<dyn FnOnce()> = Box::new(move || {
            let _ = ready_tx.send(Ok(()));
        });
        if let Err(e) = app::serve(options, Some(ready)) {
            eprintln!("server stopped: {e:#}");
            let _ = tx.send(Err(format!("{e:#}")));
        }
    });
    if let Err(e) = spawned {
        STARTED.store(false, Ordering::SeqCst);
        return Err(format!("cannot start the server thread: {e}"));
    }

    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            STARTED.store(false, Ordering::SeqCst);
            Err(e)
        }
        Err(_) => {
            STARTED.store(false, Ordering::SeqCst);
            Err("the server did not start within 15 seconds".to_string())
        }
    }
}

// ---- Logs: what the program prints on stderr goes to logcat (tag "nicochess") ----

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: libc::c_int, tag: *const libc::c_char, text: *const libc::c_char) -> libc::c_int;
}

const ANDROID_LOG_INFO: libc::c_int = 4;

fn redirect_output_to_logcat() {
    use std::io::{BufRead, BufReader};
    use std::os::fd::FromRawFd;

    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: plain libc calls on descriptors this function creates itself.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        libc::dup2(fds[1], 1);
        libc::dup2(fds[1], 2);
        libc::close(fds[1]);
    }
    let read_end = fds[0];
    let _ = std::thread::Builder::new().name("logcat".into()).spawn(move || {
        // SAFETY: `read_end` is the read side of the pipe created above, owned by this thread.
        let file = unsafe { std::fs::File::from_raw_fd(read_end) };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(text) = std::ffi::CString::new(line) {
                // SAFETY: both strings are valid and NUL-terminated.
                unsafe { __android_log_write(ANDROID_LOG_INFO, c"nicochess".as_ptr(), text.as_ptr()) };
            }
        }
    });
}
