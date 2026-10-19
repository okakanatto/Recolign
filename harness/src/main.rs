//! Development harness: serves the app's screens (app/dist) to a browser on
//! 127.0.0.1 and routes `/api/<command>` to the same `cdr-service` commands
//! the desktop app uses. Used for automated UI tests and measurements in
//! Chromium (the engine of WebView2); it is not shipped.
//!
//! Only connections from this machine are accepted, every API call must
//! carry the per-launch token, and the Host header must be 127.0.0.1 or
//! localhost.
//!
//!   cdr-harness [--ui app/dist] [--port N] [--data-dir DIR] [--memory-floor BYTES]
//!               [--left A.csv --right B.csv --key COL ... [--encoding E]]
//!
//! With --data-dir, review states are saved there (as in the desktop app).
//! --memory-floor makes comparisons stop as soon as the memory left is below
//! the given bytes (to test the out-of-memory stop).
//!
//! With --left/--right/--key the comparison starts immediately.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cdr_engine::pipeline::{CompareRequest, SideInput};
use cdr_engine::{LoadOptions, TextEncoding, TrimRule};
use cdr_service::Service;
use serde_json::Value;

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Reads one HTTP/1.1 request. `None` on EOF, timeout or malformed input.
fn read_request(r: &mut BufReader<TcpStream>) -> Option<HttpRequest> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h).ok()?;
        total += n;
        if n == 0 || total > 64 * 1024 {
            return None;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let mut req = HttpRequest { method, path, headers, body: Vec::new() };
    let len: usize = req.header("Content-Length").map_or(Ok(0), str::parse).ok()?;
    if len > 1 << 20 {
        return None;
    }
    req.body.resize(len, 0);
    r.read_exact(&mut req.body).ok()?;
    Some(req)
}

/// Writes the whole response with a single write (TCP_NODELAY is set, so
/// nothing waits for a delayed ACK).
fn respond(w: &mut TcpStream, status: u16, ctype: &str, body: &[u8], close: bool) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; connect-src 'self'; img-src 'self' data:; \
         style-src 'self'; script-src 'self'; frame-ancestors 'none'\r\n{}\r\n",
        body.len(),
        if close { "Connection: close\r\n" } else { "" }
    )
    .into_bytes();
    out.extend_from_slice(body);
    w.write_all(&out)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A file under `root` for a URL path; `None` for anything that leaves it.
fn static_file(root: &Path, url: &str) -> Option<PathBuf> {
    let rel = Path::new(url.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let p = if url == "/" { root.join("index.html") } else { root.join(rel) };
    p.is_file().then_some(p)
}

struct Harness {
    service: Arc<Service>,
    token: String,
    ui: PathBuf,
    port: u16,
}

fn serve_connection(h: &Harness, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    while let Some(req) = read_request(&mut reader) {
        let close = req.header("Connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let port = h.port;
        let host_ok =
            req.header("Host").is_some_and(|v| v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}"));
        let result = if !host_ok {
            respond(&mut writer, 403, "text/plain", b"forbidden", true)
        } else if req.method == "GET" && !req.path.starts_with("/api/") {
            match static_file(&h.ui, &req.path) {
                Some(p) => match std::fs::read(&p) {
                    Ok(body) => respond(&mut writer, 200, content_type(&p), &body, close),
                    Err(_) => respond(&mut writer, 404, "text/plain", b"not found", close),
                },
                None => respond(&mut writer, 404, "text/plain", b"not found", close),
            }
        } else if req.method != "POST" || req.header("X-CDR-Token") != Some(h.token.as_str()) {
            respond(&mut writer, 403, "text/plain", b"forbidden", close)
        } else {
            let cmd = req.path.trim_start_matches("/api/");
            let args: Value = if req.body.is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_slice(&req.body).unwrap_or(Value::Null)
            };
            let (status, body) = match h.service.handle(cmd, &args) {
                Ok(v) => (200, v),
                Err(e) => (e.status, e.body),
            };
            respond(&mut writer, status, "application/json; charset=utf-8", &serde_json::to_vec(&body).unwrap(), close)
        };
        if result.is_err() || close || !host_ok {
            break;
        }
    }
}

struct Args {
    ui: PathBuf,
    port: u16,
    start: Option<CompareRequest>,
    data_dir: Option<PathBuf>,
    memory_floor: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut left, mut right, mut keys, mut port) = (None, None, Vec::new(), 0u16);
    let mut ui = PathBuf::from("app/dist");
    let mut data_dir = None;
    let mut memory_floor = None;
    let (mut lenc, mut renc) = (TextEncoding::Utf8, TextEncoding::Utf8);
    let enc = |v: &str| match v {
        "utf8" | "utf-8" => Ok(TextEncoding::Utf8),
        "windows31j" | "cp932" => Ok(TextEncoding::Windows31j),
        _ => Err(format!("文字コード「{v}」には対応していません。")),
    };
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} の値がありません。"));
        match flag.as_str() {
            "--ui" => ui = PathBuf::from(val()?),
            "--left" => left = Some(PathBuf::from(val()?)),
            "--right" => right = Some(PathBuf::from(val()?)),
            "--key" => keys.push(val()?),
            "--encoding" => {
                let e = enc(&val()?)?;
                (lenc, renc) = (e, e);
            }
            "--left-encoding" => lenc = enc(&val()?)?,
            "--right-encoding" => renc = enc(&val()?)?,
            "--data-dir" => data_dir = Some(PathBuf::from(val()?)),
            "--memory-floor" => {
                memory_floor = Some(val()?.parse().map_err(|_| "--memory-floor にはバイト数を指定してください。")?)
            }
            "--port" => port = val()?.parse().map_err(|_| "--port には数値を指定してください。")?,
            other => return Err(format!("不明なオプション {other}")),
        }
    }
    if !ui.join("index.html").is_file() {
        return Err(format!("{}/index.html がありません。app で npm run build を実行してください。", ui.display()));
    }
    let start = match (left, right) {
        (Some(l), Some(r)) => {
            let side = |path, encoding| SideInput { path, options: LoadOptions { encoding, ..Default::default() } };
            Some(CompareRequest {
                left: side(l, lenc),
                right: side(r, renc),
                keys,
                excluded: vec![],
                trim: TrimRule::None,
            })
        }
        (None, None) => None,
        _ => return Err("--left と --right は両方指定してください。".into()),
    };
    Ok(Args { ui, port, start, data_dir, memory_floor })
}

fn random_token() -> String {
    let s = ahash::RandomState::new();
    let a = s.hash_one(std::process::id());
    let b = s.hash_one(Instant::now());
    format!("{a:016x}{b:016x}")
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\nusage: cdr-harness [--ui DIR] [--port N] [--data-dir DIR] [--memory-floor BYTES] [--left FILE --right FILE --key NAME ... [--encoding utf8|windows31j]]");
            std::process::exit(64);
        }
    };
    let listener = TcpListener::bind(("127.0.0.1", args.port)).expect("bind 127.0.0.1");
    let port = listener.local_addr().expect("address").port();
    let service = Arc::new(match args.data_dir {
        Some(d) => Service::with_data_dir(d),
        None => Service::new(),
    });
    if let Some(floor) = args.memory_floor {
        // Tests: stop any comparison once the memory left is below `floor`.
        service.set_memory_guard(floor, 0);
    }
    if let Some(req) = args.start {
        service.start(req);
    }
    let h = Arc::new(Harness { service, token: random_token(), ui: args.ui, port });
    println!("http://127.0.0.1:{port}/#t={}", h.token);
    for stream in listener.incoming().flatten() {
        let h = h.clone();
        std::thread::spawn(move || serve_connection(&h, stream));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A harness on a free port, serving a page from a temporary folder.
    fn start() -> (u16, String) {
        let ui = std::env::temp_dir().join(format!("cdr-harness-test-{}", std::process::id()));
        std::fs::create_dir_all(&ui).unwrap();
        std::fs::write(ui.join("index.html"), "<!doctype html>").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = Arc::new(Harness { service: Arc::new(Service::new()), token: random_token(), ui, port });
        let token = h.token.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let h = h.clone();
                std::thread::spawn(move || serve_connection(&h, stream));
            }
        });
        (port, token)
    }

    /// Sends one request and returns the status code of the reply.
    fn status(port: u16, request: &str) -> u16 {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
    }

    #[test]
    fn only_this_machine_with_the_token_reaches_the_commands() {
        let (port, token) = start();
        let host = format!("127.0.0.1:{port}");
        let api = |host: &str, token: Option<&str>| {
            let t = token.map_or(String::new(), |t| format!("X-CDR-Token: {t}\r\n"));
            format!("POST /api/status HTTP/1.1\r\nHost: {host}\r\n{t}Content-Length: 0\r\nConnection: close\r\n\r\n")
        };
        assert_eq!(status(port, &api(&host, Some(&token))), 200);
        assert_eq!(status(port, &api(&format!("localhost:{port}"), Some(&token))), 200);
        assert_eq!(status(port, &api(&host, None)), 403, "no token");
        assert_eq!(status(port, &api(&host, Some("0123"))), 403, "wrong token");
        // A page from another site that resolves its name to 127.0.0.1.
        assert_eq!(status(port, &api(&format!("example.com:{port}"), Some(&token))), 403, "other host");
        let get =
            format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\nX-CDR-Token: {token}\r\nConnection: close\r\n\r\n");
        assert_eq!(status(port, &get), 403, "commands are POST only");
    }

    #[test]
    fn files_outside_the_page_folder_are_not_served() {
        let (port, _) = start();
        let get = |path: &str, host: &str| format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let host = format!("127.0.0.1:{port}");
        assert_eq!(status(port, &get("/", &host)), 200);
        assert_eq!(status(port, &get("/", "example.com")), 403);
        for path in ["/../Cargo.toml", "/./index.html", "//etc/passwd", "/%2e%2e/Cargo.toml", "/C:/Windows/win.ini"] {
            assert_eq!(status(port, &get(path, &host)), 404, "{path}");
        }
    }

    #[test]
    fn oversized_requests_are_dropped() {
        let (port, token) = start();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!(
            "POST /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-CDR-Token: {token}\r\nContent-Length: {}\r\n\r\n",
            (1 << 20) + 1
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut reply = Vec::new();
        let _ = s.read_to_end(&mut reply);
        assert!(reply.is_empty(), "no reply, the connection is closed");
    }
}
