//! A small HTTP/1.1 server on the standard library, built for this project.
//!
//! Why not a ready-made one: pages need server-sent events, which means pushing
//! small messages that must leave the machine at once. The usual small servers
//! buffer their output, so events would arrive late and in bursts.
//!
//! What it does: one thread per connection, `Connection: close` on everything,
//! limits on header and body size, and timeouts. What it does not: keep-alive,
//! HTTPS, chunked request bodies.

use anyhow::{Context, Result};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The value of a query parameter, as sent (no percent-decoding: the page only
    /// sends hexadecimal tokens and plain ids).
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    #[cfg(test)]
    pub fn for_test(method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: body.as_bytes().to_vec(),
        }
    }
}

/// Writes an open-ended response until the client goes away.
pub type StreamBody = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

pub enum Reply {
    Bytes {
        status: u16,
        content_type: String,
        body: Vec<u8>,
        extra_headers: Vec<(String, String)>,
    },
    /// An open-ended response (server-sent events): the function writes until the
    /// client goes away, which shows up as a write error.
    Stream {
        content_type: String,
        run: StreamBody,
    },
}

impl Reply {
    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Reply {
        Reply::Bytes { status, content_type: content_type.to_string(), body, extra_headers: Vec::new() }
    }

    pub fn text(status: u16, text: &str) -> Reply {
        Reply::bytes(status, "text/plain; charset=utf-8", text.as_bytes().to_vec())
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Reply {
        Reply::bytes(status, "application/json", value.to_string().into_bytes())
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Reply {
        if let Reply::Bytes { extra_headers, .. } = &mut self {
            extra_headers.push((name.to_string(), value.to_string()));
        }
        self
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Reply + Send + Sync>;

/// Serves forever. At most `max_connections` connections are handled at once;
/// the others get a 503. `on_ready` runs once the port is open.
pub fn serve(
    addr: &str,
    max_connections: usize,
    handler: Handler,
    on_ready: Option<Box<dyn FnOnce()>>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    serve_on(listener, max_connections, handler, on_ready)
}

/// Binds the first address of the list that works (an address can be taken, or reserved by
/// the system, as Docker and Hyper-V do on Windows). Says which one it took.
pub fn bind_first(addresses: &[String]) -> Result<TcpListener> {
    let mut last = None;
    for addr in addresses {
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(e) => last = Some(anyhow::Error::new(e).context(format!("cannot listen on {addr}"))),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no address to listen on")))
}

/// Like [`serve`], on a listener that is already open.
pub fn serve_on(
    listener: TcpListener,
    max_connections: usize,
    handler: Handler,
    on_ready: Option<Box<dyn FnOnce()>>,
) -> Result<()> {
    let shown = listener.local_addr().map_or_else(|_| "?".to_string(), |a| a.to_string());
    eprintln!("http: listening on http://{shown}");
    if let Some(on_ready) = on_ready {
        on_ready();
    }
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        accept(stream, max_connections, &handler, &active);
    }
    Ok(())
}

/// Hands a new connection to a thread of its own, or refuses it when too many are open.
fn accept(mut stream: TcpStream, max_connections: usize, handler: &Handler, active: &Arc<AtomicUsize>) {
    if active.fetch_add(1, Ordering::SeqCst) >= max_connections {
        active.fetch_sub(1, Ordering::SeqCst);
        let _ = write_bytes(&mut stream, 503, "text/plain", b"busy", &[]);
        close_politely(&mut stream);
        return;
    }
    let (handler, active) = (Arc::clone(handler), Arc::clone(active));
    let spawned = thread::Builder::new().name("http".into()).stack_size(512 * 1024).spawn({
        let active = Arc::clone(&active);
        move || {
            let _ = connection(stream, &handler);
            active.fetch_sub(1, Ordering::SeqCst);
        }
    });
    if spawned.is_err() {
        // Could not start a thread: give the slot back.
        active.fetch_sub(1, Ordering::SeqCst);
        eprintln!("http: cannot start a connection thread");
    }
}

/// A server that can be stopped: the Android app opens one for other devices when the
/// owner switches that on, and closes it when they switch it off.
pub struct Listener {
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Listener {
    /// The address it listens on (with the real port when 0 was asked for).
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Stops accepting. Connections already open (event streams) end by themselves when
    /// their page goes away.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts serving on a thread of its own and returns at once. The listener is checked for
/// new connections ten times a second so that it can be stopped.
pub fn spawn(addr: &str, max_connections: usize, handler: Handler) -> Result<Listener> {
    let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    eprintln!("http: also listening on http://{local}");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    thread::Builder::new().name("http-accept".into()).spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Some systems hand on the non-blocking mode of the listener.
                    if stream.set_nonblocking(false).is_ok() {
                        accept(stream, max_connections, &handler, &active);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
        eprintln!("http: stopped listening on http://{local}");
    })?;
    Ok(Listener { stop, addr: local })
}

fn connection(mut stream: TcpStream, handler: &Handler) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_nodelay(true).ok();

    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(ReadError::Io(_)) => return Ok(()),
        Err(ReadError::Bad(status, why)) => {
            let written = write_bytes(&mut stream, status, "text/plain", why.as_bytes(), &[]);
            close_politely(&mut stream);
            return written;
        }
    };

    // A page must never get an old answer from the browser's cache.
    match handler(request) {
        Reply::Bytes { status, content_type, body, extra_headers } => {
            write_bytes(&mut stream, status, &content_type, &body, &extra_headers)
        }
        Reply::Stream { content_type, run } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\n\
                 Connection: close\r\nX-Accel-Buffering: no\r\n\r\n"
            )?;
            stream.flush()?;
            // Events can wait a long time: no read timeout is needed, and the write
            // timeout above ends the stream when a client has vanished.
            run(&mut stream)
        }
    }
}

/// Ends a connection whose request was not fully read. Closing it right away would
/// make the operating system reset the connection, and the client could lose the
/// reply: so say we are done writing, and give the client a moment to finish.
fn close_politely(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut sink = [0u8; 2048];
    let _ = stream.read(&mut sink);
}

enum ReadError {
    Io(#[allow(dead_code)] io::Error),
    Bad(u16, &'static str),
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ReadError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(head_len)) => {
                let method = parsed.method.unwrap_or("").to_string();
                let target = parsed.path.unwrap_or("/").to_string();
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string()))
                    .collect();

                let length = match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
                    Some((_, v)) => v.parse::<usize>().map_err(|_| ReadError::Bad(400, "bad Content-Length"))?,
                    None => 0,
                };
                if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")) {
                    return Err(ReadError::Bad(501, "chunked bodies are not supported"));
                }
                if length > MAX_BODY {
                    return Err(ReadError::Bad(413, "body too large"));
                }

                let mut body = buf[head_len..].to_vec();
                while body.len() < length {
                    let n = stream.read(&mut chunk).map_err(ReadError::Io)?;
                    if n == 0 {
                        return Err(ReadError::Bad(400, "body cut short"));
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                body.truncate(length);

                let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
                return Ok(Request { method, path: path.to_string(), query: query.to_string(), headers, body });
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => return Err(ReadError::Bad(400, "bad request")),
        }
        if buf.len() >= MAX_HEAD {
            return Err(ReadError::Bad(431, "headers too large"));
        }
        let n = stream.read(&mut chunk).map_err(ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_bytes(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(String, String)],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n",
        reason(status),
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    /// Starts a server on a free port and returns its address.
    fn start(handler: Handler) -> String {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        let a = addr.clone();
        thread::spawn(move || serve(&a, 8, handler, None).unwrap());
        for _ in 0..50 {
            if TcpStream::connect(&addr).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        addr
    }

    fn exchange(addr: &str, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn a_stoppable_server_serves_and_then_stops() {
        let handler: Handler = Arc::new(|_| Reply::text(200, "hello"));
        let listener = spawn("127.0.0.1:0", 8, handler).unwrap();
        let addr = listener.local_addr().to_string();
        assert!(exchange(&addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").ends_with("hello"));
        listener.stop();
        // New connections are refused soon after.
        let started = std::time::Instant::now();
        while TcpStream::connect(&addr).is_ok() {
            assert!(started.elapsed() < Duration::from_secs(3), "still listening after stop");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn the_first_free_address_is_taken() {
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = busy.local_addr().unwrap().to_string();
        let listener = bind_first(&[taken.clone(), "127.0.0.1:0".to_string()]).unwrap();
        assert_ne!(listener.local_addr().unwrap().to_string(), taken);
        assert!(bind_first(&[taken]).is_err());
    }

    #[test]
    fn serves_requests_with_body_query_and_headers() {
        let handler: Handler = Arc::new(|req: Request| {
            let echo = format!(
                "{} {} q={:?} token={:?} body={}",
                req.method,
                req.path,
                req.query_param("a"),
                req.header("x-client-token"),
                String::from_utf8_lossy(&req.body)
            );
            Reply::text(200, &echo).with_header("X-Test", "yes")
        });
        let addr = start(handler);
        let out = exchange(
            &addr,
            "POST /api/x?a=1&b=2 HTTP/1.1\r\nHost: t\r\nX-Client-Token: abc\r\nContent-Length: 5\r\n\r\nhello",
        );
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.contains("X-Test: yes"));
        assert!(out.contains("POST /api/x q=Some(\"1\") token=Some(\"abc\") body=hello"), "{out}");
    }

    #[test]
    fn rejects_bad_oversized_and_chunked_requests() {
        let addr = start(Arc::new(|_| Reply::text(200, "ok")));
        assert!(exchange(&addr, "garbage\r\n\r\n").contains("400"));
        let big = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(exchange(&addr, &big).contains("413"));
        assert!(exchange(&addr, "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").contains("501"));
    }

    #[test]
    fn streams_events_as_they_are_written() {
        let handler: Handler = Arc::new(|_| Reply::Stream {
            content_type: "text/event-stream".into(),
            run: Box::new(|w| {
                w.write_all(b"data: one\n\n")?;
                w.flush()?;
                thread::sleep(Duration::from_millis(300));
                w.write_all(b"data: two\n\n")?;
                w.flush()
            }),
        });
        let addr = start(handler);
        let mut stream = TcpStream::connect(&addr).unwrap();
        stream.write_all(b"GET /events HTTP/1.1\r\n\r\n").unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let started = std::time::Instant::now();
        let mut first_at = None;
        while reader.read_line(&mut line).unwrap() > 0 {
            if line.starts_with("data: one") {
                first_at = Some(started.elapsed());
            }
            if line.starts_with("data: two") {
                break;
            }
            line.clear();
        }
        // The first event must arrive well before the second is written.
        assert!(first_at.unwrap() < Duration::from_millis(250), "{first_at:?}");
    }

    #[test]
    fn busy_server_answers_503() {
        let handler: Handler = Arc::new(|_| Reply::Stream {
            content_type: "text/event-stream".into(),
            run: Box::new(|w| {
                w.write_all(b": hi\n\n")?;
                w.flush()?;
                thread::sleep(Duration::from_millis(600));
                Ok(())
            }),
        });
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        let a = addr.clone();
        thread::spawn(move || serve(&a, 1, handler, None).unwrap());
        thread::sleep(Duration::from_millis(150));

        let mut first = TcpStream::connect(&addr).unwrap();
        first.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        thread::sleep(Duration::from_millis(150));
        let out = exchange(&addr, "GET / HTTP/1.1\r\n\r\n");
        assert!(out.contains("503"), "{out}");
    }
}
