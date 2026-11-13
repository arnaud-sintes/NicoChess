//! The Android side of NicoChess: the core library behind a JNI entry point.
//!
//! The app (Kotlin) calls `NativeCore.start(...)`. That starts the web server in a
//! thread of this process, on the loopback address, and the app shows the page in a
//! WebView. `NativeCore.setRemote(...)` opens or closes a second listener on the
//! network, so that other devices can watch (or control) from their own browser.

use jni::JNIEnv;
use jni::objects::{JByteBuffer, JClass, JString};
use jni::sys::{jboolean, jint, jstring};
use nicochess::app::{self, ServerOptions};
use nicochess::camera::SourceSpec;
use nicochess::engine::Engine;
use nicochess::http::{self, Handler, Listener};
use nicochess::pushed::{FrameInbox, yuv420_to_rgb};
use nicochess::tracker::TrackerConfig;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock, mpsc};
use std::time::Duration;

/// Held during the start, so that a second caller waits for the first one instead of believing
/// the server is ready.
static UP: Mutex<()> = Mutex::new(());
/// Set when the server accepts requests (also if that took longer than `start` waited).
static READY: AtomicBool = AtomicBool::new(false);
/// The engine, to ask whether a game is running.
static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();
/// What the listeners serve, and how many connections each may hold.
static SERVER: OnceLock<(Handler, usize)> = OnceLock::new();
/// The listener open to the network, while the owner allows it.
static REMOTE: Mutex<Option<Listener>> = Mutex::new(None);

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
    let mut read = |s: &JString<'l>| -> Result<String, String> { env.get_string(s).map(String::from).map_err(|e| e.to_string()) };
    let result = match (read(&data_dir), read(&listen), read(&source)) {
        (Ok(d), Ok(l), Ok(s)) => {
            std::panic::catch_unwind(|| start(d, l, s)).unwrap_or_else(|_| Err("the native code panicked".to_string()))
        }
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
pub extern "system" fn Java_com_nicochess_app_NativeCore_cameraFailed<'l>(mut env: JNIEnv<'l>, _class: JClass<'l>, message: JString<'l>) {
    let text = env.get_string(&message).map(String::from).unwrap_or_else(|_| "the camera failed".to_string());
    guarded(|| inbox().fail(text));
}

/// `String NativeCore.setRemote(boolean enabled, int port)`: opens (or closes) the listener on all
/// the addresses of the phone. Returns "" or the reason it did not work.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_setRemote<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    enabled: jboolean,
    port: jint,
) -> jstring {
    let result =
        std::panic::catch_unwind(|| set_remote(enabled != 0, port)).unwrap_or_else(|_| Err("the native code panicked".to_string()));
    env.new_string(result.err().unwrap_or_default()).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn set_remote(enabled: bool, port: i32) -> Result<(), String> {
    let Some((handler, max_connections)) = SERVER.get() else {
        return Err("the server is not running yet".to_string());
    };
    let port = u16::try_from(port).ok().filter(|p| *p >= 1024).ok_or("the port must be between 1024 and 65535")?;
    let mut remote = REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    if !enabled {
        *remote = None; // dropping the listener stops it
        return Ok(());
    }
    if remote.as_ref().is_some_and(|l| l.local_addr().port() == port) {
        return Ok(());
    }
    *remote = None;
    // A listener that was just stopped lets go of its port within a fraction of a second.
    let mut last = String::new();
    for _ in 0..10 {
        match http::spawn(&format!("0.0.0.0:{port}"), *max_connections, handler.clone()) {
            Ok(listener) => {
                *remote = Some(listener);
                return Ok(());
            }
            Err(e) => last = format!("{e:#}"),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(last)
}

/// `boolean NativeCore.isPlaying()`: a game is being recorded (the app then refuses to change camera).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_nicochess_app_NativeCore_isPlaying<'l>(_env: JNIEnv<'l>, _class: JClass<'l>) -> jboolean {
    jboolean::from(ENGINE.get().is_some_and(|engine| engine.is_playing()))
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
        guarded(|| inbox().fail("the camera gave pictures that cannot be read (no direct buffers)"));
        return;
    };
    let dimensions = (u32::try_from(width), u32::try_from(height));
    let strides = (usize::try_from(y_stride), usize::try_from(uv_stride), usize::try_from(uv_pixel_stride));
    let (Ok(width), Ok(height), Ok(y_stride), Ok(uv_stride), Ok(uv_pixel_stride)) =
        (dimensions.0, dimensions.1, strides.0, strides.1, strides.2)
    else {
        guarded(|| inbox().fail("the camera gave a picture with a negative size or stride"));
        return;
    };
    // A panic must not cross into the JVM (it would abort the app); one bad picture is skipped, not
    // fatal: the next one is probably fine, and failing would close and reopen the camera.
    match std::panic::catch_unwind(|| yuv420_to_rgb(width, height, y, y_stride, u, v, uv_stride, uv_pixel_stride)) {
        Ok(Ok(image)) => guarded(|| inbox().push(image)),
        Ok(Err(e)) => eprintln!("camera: a picture was skipped: {e:#}"),
        Err(_) => eprintln!("camera: a picture was skipped (internal error)"),
    }
}

/// Runs `f`; a panic is logged and stops here: it must not unwind into the JVM, which would abort the app.
fn guarded(f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        eprintln!("camera: internal error (a panic was stopped before it reached the app)");
    }
}

fn start(data_dir: String, listen: String, source: String) -> Result<(), String> {
    let _starting = UP.lock().unwrap_or_else(|e| e.into_inner());
    if READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    static LOGS: Once = Once::new();
    LOGS.call_once(redirect_output_to_logcat);

    let spec = match source.as_str() {
        "phone" => SourceSpec::Pushed(inbox()),
        "demo" => SourceSpec::Demo { move_interval: Duration::from_secs(4) },
        "synthetic" => SourceSpec::Synthetic,
        other => return Err(format!("unknown source \"{other}\" (this build has: phone, demo, synthetic)")),
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
        // Take the port first: if it is not free, nothing else has been built yet.
        let listener = match http::bind_first(&[options.listen.clone()]) {
            Ok(listener) => listener,
            Err(e) => {
                let _ = tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        let built = match app::build(options) {
            Ok(built) => built,
            Err(e) => {
                let _ = tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        let _ = SERVER.set((built.handler.clone(), built.max_connections));
        let _ = ENGINE.set(Arc::clone(&built.engine));
        let ready: Box<dyn FnOnce()> = Box::new(move || {
            READY.store(true, Ordering::SeqCst);
            let _ = ready_tx.send(Ok(()));
        });
        if let Err(e) = http::serve_on(listener, built.max_connections, built.handler, Some(ready)) {
            eprintln!("http: server stopped: {e:#}");
            let _ = tx.send(Err(format!("{e:#}")));
        }
    });
    if let Err(e) = spawned {
        return Err(format!("cannot start the server thread: {e}"));
    }

    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the server did not start within 15 seconds".to_string()),
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
        for line in BufReader::new(file).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Ok(text) = std::ffi::CString::new(line) {
                // SAFETY: both strings are valid and NUL-terminated.
                unsafe { __android_log_write(ANDROID_LOG_INFO, c"nicochess".as_ptr(), text.as_ptr()) };
            }
        }
    });
}
