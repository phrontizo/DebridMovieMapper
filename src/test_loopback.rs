//! A scripted loopback HTTP/1.1 server for the HTTP clients' tests, shared by every module's tests
//! (as [`crate::test_log`] is). [`serve`] runs one [`Step`] per accepted connection: answer the
//! request, close the connection before or after reading it, or hold it open without answering.
//! [`serve_fn`] answers every request with a reply built from its text. Both count the connections
//! they accept and record each request they read. Every reply carries `Connection: close`, so each
//! request the client sends arrives on a connection of its own and takes the next step.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One HTTP reply. Its status line carries the status's canonical reason phrase.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub status: u16,
    /// Written before the `Content-Length` and `Connection: close` every reply carries.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    /// `status` with a JSON `body` (`Content-Type: application/json`).
    pub(crate) fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.into(),
        }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// What [`serve`] does with one accepted connection.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Read the request, then answer it.
    Reply(Reply),
    /// Close the connection at once, before reading the request: the client sees a reset or an
    /// EOF before any response.
    DropBeforeRead,
    /// Read the request, then close the connection without answering it.
    DropAfterRead,
    /// Read the request, then never answer: hold the connection until the client closes it (its
    /// request timeout). The server waits on a read, not a sleep, so a paused test clock can't end
    /// the hang early; it serves the next connection only once this one is closed.
    HangUntilClosed,
}

impl Step {
    /// A bare `status`: an empty body and no `Content-Type`.
    pub(crate) fn status(status: u16) -> Self {
        Step::Reply(Reply {
            status,
            headers: Vec::new(),
            body: String::new(),
        })
    }

    /// `status` with a JSON `body` ([`Reply::json`]).
    pub(crate) fn json(status: u16, body: impl Into<String>) -> Self {
        Step::Reply(Reply::json(status, body))
    }
}

/// A server started by [`serve`] or [`serve_fn`].
pub(crate) struct Loopback {
    /// `http://<addr>`, with no trailing slash.
    pub base: String,
    /// Connections accepted so far.
    pub connections: Arc<AtomicUsize>,
    /// Each request read so far (head and body), in arrival order. A connection closed by
    /// [`Step::DropBeforeRead`] records nothing.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl Loopback {
    /// The head (request line and headers) of each request read so far.
    pub(crate) fn heads(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.split("\r\n\r\n").next().unwrap_or_default().to_string())
            .collect()
    }
}

async fn bind() -> (TcpListener, Loopback) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Loopback {
        base: format!("http://{addr}"),
        connections: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    (listener, server)
}

/// Serve `steps` to successive connections, one step per connection, in order. Once they are used
/// up, every later connection gets `then`; with `None` the server stops accepting, so a later
/// connect is refused.
pub(crate) async fn serve(steps: Vec<Step>, then: Option<Step>) -> Loopback {
    let (listener, server) = bind().await;
    let (count, log) = (server.connections.clone(), server.requests.clone());
    tokio::spawn(async move {
        for step in steps.into_iter().chain(then.into_iter().cycle()) {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            count.fetch_add(1, Ordering::SeqCst);
            if matches!(step, Step::DropBeforeRead) {
                continue;
            }
            let req = read_request(&mut sock).await;
            log.lock().unwrap().push(req);
            match step {
                Step::Reply(reply) => write_reply(&mut sock, &reply).await,
                Step::HangUntilClosed => {
                    let mut sink = [0u8; 1024];
                    while matches!(sock.read(&mut sink).await, Ok(n) if n > 0) {}
                }
                Step::DropBeforeRead | Step::DropAfterRead => {}
            }
        }
    });
    server
}

/// Answer every connection with the reply `respond` builds from its request (head and body).
pub(crate) async fn serve_fn(respond: impl Fn(&str) -> Reply + Send + 'static) -> Loopback {
    let (listener, server) = bind().await;
    let (count, log) = (server.connections.clone(), server.requests.clone());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            let req = read_request(&mut sock).await;
            let reply = respond(&req);
            log.lock().unwrap().push(req);
            write_reply(&mut sock, &reply).await;
        }
    });
    server
}

/// Read one request from `sock`: its head and as much body as its `Content-Length` declares (an
/// unread body would make the close reset the connection under the client). Returns what arrived
/// before an EOF or a read error, if one comes first.
pub(crate) async fn read_request(sock: &mut TcpStream) -> String {
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = sock.read(&mut buf).await.unwrap_or(0);
        req.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&req);
        let complete = text.split_once("\r\n\r\n").is_some_and(|(head, rest)| {
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            rest.len() >= len
        });
        if n == 0 || complete {
            return text.into_owned();
        }
    }
}

/// Write `reply` with `Connection: close`, then flush.
pub(crate) async fn write_reply(sock: &mut TcpStream, reply: &Reply) {
    let reason = reqwest::StatusCode::from_u16(reply.status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("STATUS");
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(reply.body.as_bytes()).await;
    let _ = sock.flush().await;
}
