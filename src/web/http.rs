//! Small, bounded HTTP/1.1 reader. One request per connection; no streaming or
//! filesystem routes. Reject ambiguous framing instead of trying to recover.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const HEADER_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = 64 * 1024;

pub(super) struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

pub(super) struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({"error": message.into()}))
    }

    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn write(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Content Too Large",
            431 => "Request Header Fields Too Large",
            409 => "Conflict",
            _ => "Internal Server Error",
        };
        let header = format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'\r\nCache-Control: no-store\r\n{}\r\n",
            self.status, reason, self.content_type, self.body.len(),
            if self.status == 405 { "Allow: GET, POST\r\n" } else { "" });
        // Write the response together: closing a rejected request with unread
        // input can reset the socket, so do not split its small error header
        // into many separate writes.
        let mut response = header.into_bytes();
        response.extend_from_slice(&self.body);
        stream.write_all(&response)
    }
}

pub(super) fn serve(
    mut stream: TcpStream,
    shutdown: &AtomicBool,
    port: u16,
    route: impl FnOnce(Request) -> Response,
) {
    if stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .is_err()
    {
        return;
    }
    let response = match read_request(&mut stream, shutdown, port) {
        Ok(request) if !shutdown.load(Ordering::Relaxed) => route(request),
        Ok(_) => return,
        Err(response) => response,
    };
    if !shutdown.load(Ordering::Relaxed) {
        let _ = response.write(&mut stream);
    }
}

fn bad(message: &str) -> Response {
    Response::error(400, message)
}

fn read_request(
    stream: &mut TcpStream,
    shutdown: &AtomicBool,
    port: u16,
) -> Result<Request, Response> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    let mut chunk = [0; 2048];
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            if index + 4 > HEADER_LIMIT {
                return Err(Response::error(431, "Headers too large"));
            }
            break index + 4;
        }
        if bytes.len() >= HEADER_LIMIT {
            return Err(Response::error(431, "Headers too large"));
        }
        read_chunk(stream, shutdown, deadline, &mut chunk, &mut bytes)?;
    };
    let head = std::str::from_utf8(&bytes[..header_end]).map_err(|_| bad("Invalid headers"))?;
    let mut lines = head[..head.len() - 4].split("\r\n");
    let mut line = lines.next().unwrap_or_default().split(' ');
    let method = line.next().unwrap_or_default();
    let target = line.next().unwrap_or_default();
    if line.next() != Some("HTTP/1.1")
        || line.next().is_some()
        || !target.starts_with('/')
        || target.contains('#')
        || target.bytes().any(|c| c.is_ascii_control())
    {
        return Err(bad("Malformed HTTP/1.1 request line"));
    }
    let method = method.to_string();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(path)?;
    // Queries are currently unused, but malformed escapes must still be rejected.
    percent_decode(query)?;
    let mut length = None;
    let mut host = None;
    let mut origin = None;
    let mut content_type = None;
    let mut fetch_site = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad("Malformed header"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
            || value.bytes().any(|c| c.is_ascii_control() && c != b'\t')
        {
            return Err(bad("Malformed header"));
        }
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "transfer-encoding" => return Err(bad("Transfer-Encoding is not supported")),
            "content-length" => {
                if length.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|c| c.is_ascii_digit())
                {
                    return Err(bad("Invalid Content-Length"));
                }
                length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| bad("Invalid Content-Length"))?,
                );
            }
            "host" => {
                if host.replace(value).is_some() {
                    return Err(bad("Duplicate Host"));
                }
            }
            "origin" => {
                if origin.replace(value).is_some() {
                    return Err(bad("Duplicate Origin"));
                }
            }
            "content-type" => {
                if content_type.replace(value).is_some() {
                    return Err(bad("Duplicate Content-Type"));
                }
            }
            "sec-fetch-site" => {
                if fetch_site.replace(value).is_some() {
                    return Err(bad("Duplicate Sec-Fetch-Site"));
                }
            }
            _ => {}
        }
    }
    let host = host.ok_or_else(|| bad("Host is required"))?;
    // Accept only literal IPv4 addresses or localhost at this listener's port.
    // This also prevents an attacker's DNS name from rebinding to the server.
    let (hostname, host_port) = host.split_once(':').unwrap_or((host, "80"));
    if host_port.parse::<u16>().ok() != Some(port)
        || !(hostname.eq_ignore_ascii_case("localhost")
            || hostname.parse::<std::net::Ipv4Addr>().is_ok())
    {
        return Err(Response::error(
            403,
            "Use the LAN or localhost URL shown in Runki",
        ));
    }
    if origin.is_some_and(|origin| origin != format!("http://{host}"))
        || fetch_site.is_some_and(|site| site != "same-origin" && site != "none")
    {
        return Err(Response::error(
            403,
            "Cross-origin requests are not allowed",
        ));
    }
    if method != "GET" && method != "POST" {
        return Err(Response::error(405, "Method not allowed"));
    }
    let length = length.unwrap_or(0);
    if length > BODY_LIMIT {
        return Err(Response::error(413, "Body too large"));
    }
    // Browser form submissions cannot mutate state without a same-origin JSON
    // request. Empty action bodies are allowed (and still checked above).
    if method == "POST"
        && length > 0
        && !content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Err(bad("POST bodies must use application/json"));
    }
    while bytes.len() - header_end < length {
        read_chunk(stream, shutdown, deadline, &mut chunk, &mut bytes)?;
    }
    Ok(Request {
        method,
        path,
        body: bytes[header_end..header_end + length].to_vec(),
    })
}

fn read_chunk(
    stream: &mut TcpStream,
    shutdown: &AtomicBool,
    deadline: Instant,
    chunk: &mut [u8],
    bytes: &mut Vec<u8>,
) -> Result<(), Response> {
    loop {
        if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(bad("Request timed out"));
        }
        match stream.read(chunk) {
            Ok(0) => return Err(bad("Incomplete request")),
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                return Ok(());
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(bad("Could not read request")),
        }
    }
}

fn percent_decode(value: &str) -> Result<String, Response> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| (b as char).to_digit(16));
            let low = bytes.next().and_then(|b| (b as char).to_digit(16));
            decoded.push(match (high, low) {
                (Some(high), Some(low)) => (high * 16 + low) as u8,
                _ => return Err(bad("Malformed percent escape")),
            });
        } else {
            decoded.push(byte);
        }
    }
    if decoded.iter().any(|b| b.is_ascii_control()) {
        return Err(bad("Control character in URL"));
    }
    String::from_utf8(decoded).map_err(|_| bad("URL must be UTF-8"))
}
