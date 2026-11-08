//! Loopback HTTP server for `bitty upgrade` and `install.sh` tests.
//!
//! The upgrade path accepts a plain-HTTP base only on a loopback host (the
//! documented test seam), so the suite binds `127.0.0.1:0` and serves fixed
//! routes: no real network, no external host, no port collision.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One canned response.
pub struct Route {
    /// HTTP status code.
    pub status: u16,
    /// Extra headers (`Location`, `Content-Type`, ...).
    pub headers: Vec<(&'static str, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// Overrides `Content-Length` (the oversized-body case: curl refuses on
    /// the declared length without the test sending 1 GB).
    pub declared_length: Option<u64>,
}

impl Route {
    /// `200 OK` with a body.
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            declared_length: None,
        }
    }

    /// `302 Found` to `location` (used for the non-HTTPS redirect refusal).
    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: 302,
            headers: vec![("Location", location.into())],
            body: Vec::new(),
            declared_length: None,
        }
    }

    /// `200 OK` that declares a body far beyond any client cap.
    pub fn oversized(declared_length: u64) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
            declared_length: Some(declared_length),
        }
    }
}

/// A loopback server bound to an ephemeral port.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts serving `routes` (path -> response) on `127.0.0.1:0`.
    pub fn start(routes: Vec<(String, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let routes: HashMap<String, Route> = routes.into_iter().collect();
        let routes = Arc::new(routes);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &routes),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    /// Base URL (`http://127.0.0.1:<port>`).
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so the thread observes the flag immediately.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads one request and writes one response, then closes.
fn serve(mut stream: TcpStream, routes: &HashMap<String, Route>) {
    // BSD/macOS `accept()` inherits `O_NONBLOCK` from the listener; restore
    // blocking mode so `read` honours `set_read_timeout` instead of failing
    // immediately with `WouldBlock` before `curl` sends the HTTP request.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if request.len() > 16 * 1024 {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&request);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|target| target.split('?').next().unwrap_or(target).to_string())
        .unwrap_or_default();
    let (status, headers, body) = match routes.get(&path) {
        Some(route) => {
            let mut headers = route.headers.clone();
            headers.push((
                "Content-Length",
                route
                    .declared_length
                    .unwrap_or(route.body.len() as u64)
                    .to_string(),
            ));
            (route.status, headers, route.body.clone())
        }
        None => (404, vec![("Content-Length", "0".to_string())], Vec::new()),
    };
    let reason = match status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Error",
    };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}
