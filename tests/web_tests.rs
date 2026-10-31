use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use runki::progress;
use runki::web::{LibrarySlot, WebServer};
use serde_json::{json, Value};

struct Reply {
    status: u16,
    headers: String,
    body: String,
}
impl Reply {
    fn json(&self) -> Value {
        assert!(self.headers.contains("Content-Type: application/json"));
        serde_json::from_str(&self.body).unwrap()
    }
}

fn start(library: Option<PathBuf>) -> (WebServer, LibrarySlot) {
    let slot = Arc::new(RwLock::new(library));
    (WebServer::start(Arc::clone(&slot), 0).unwrap(), slot)
}
fn raw(server: &WebServer, request_text: &str) -> Reply {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, server.local_addr.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request_text.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    // Oversized requests may close with unread input and reset the connection
    // after their response. Preserve the response bytes in that case.
    if let Err(err) = stream.read_to_end(&mut bytes) {
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
    }
    let text = String::from_utf8(bytes).unwrap();
    let (headers, body) = text.split_once("\r\n\r\n").unwrap_or_else(|| {
        panic!(
            "No HTTP response for {:?}: {text:?}",
            &request_text[..request_text.len().min(160)]
        )
    });
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(headers.contains("Connection: close"));
    assert!(headers.contains("X-Content-Type-Options: nosniff"));
    assert!(headers.contains("Content-Security-Policy: default-src 'self'"));
    assert!(headers.contains("Cache-Control: no-store"));
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(length, body.len());
    Reply {
        status,
        headers: headers.into(),
        body: body.into(),
    }
}
fn request(server: &WebServer, method: &str, path: &str, body: Option<Value>) -> Reply {
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    raw(server, &format!("{method} {path} HTTP/1.1\r\nHost: localhost:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", server.local_addr.port(), body.len()))
}
fn post(server: &WebServer, path: &str, body: Value) -> Value {
    let reply = request(server, "POST", path, Some(body));
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    std::fs::write(
        path.join("deck.tsv"),
        "front\tback\nFirst question\tFirst answer\nSecond question\tSecond answer\n",
    )
    .unwrap();
    (dir, path)
}

#[test]
fn server_serves_page_status_and_errors_and_releases_port() {
    let (mut server, _) = start(None);
    assert!(server
        .lan_url()
        .ends_with(&format!(":{}/", server.local_addr.port())));
    assert_eq!(
        server.localhost_url(),
        format!("http://localhost:{}/", server.local_addr.port())
    );
    let page = request(&server, "GET", "/", None);
    assert_eq!(page.status, 200);
    assert!(page
        .headers
        .contains("Content-Type: text/html; charset=utf-8"));
    assert!(page.body.contains("<title>Runki</title>"));
    assert!(!page.headers.contains("Access-Control-Allow-Origin"));
    let status = request(&server, "GET", "/api/status", None).json();
    assert_eq!(
        status,
        json!({"version":runki::VERSION, "has_library":false, "deck_open":false})
    );
    let unknown = request(&server, "GET", "/unknown", None);
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["error"], "Unknown route");
    for (method, path) in [("DELETE", "/"), ("POST", "/"), ("GET", "/api/session/open")] {
        assert_eq!(request(&server, method, path, None).status, 405);
    }
    let port = server.local_addr.port();
    server.stop().unwrap();
    server.stop().unwrap();
    let _rebound = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
}

#[test]
fn rejects_malformed_oversized_and_ambiguous_requests() {
    let (server, _) = start(None);
    let host = format!("Host: localhost:{}\r\n", server.local_addr.port());
    for text in [
        "nonsense\r\n\r\n".into(),
        format!("GET / HTTP/1.0\r\n{host}\r\n"),
        format!("GET /bad%xy HTTP/1.1\r\n{host}\r\n"),
        format!("GET /?q=% HTTP/1.1\r\n{host}\r\n"),
        format!("GET /%00 HTTP/1.1\r\n{host}\r\n"),
        format!("GET / HTTP/1.1\r\n{host}broken\r\n\r\n"),
        format!("POST /api/session/open HTTP/1.1\r\n{host}Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
        format!("GET / HTTP/1.1\r\n{host}Content-Length: 0\r\nContent-Length: 0\r\n\r\n"),
        format!("POST /api/session/open HTTP/1.1\r\n{host}Content-Length: -1\r\n\r\n"),
        format!("POST /api/session/open HTTP/1.1\r\n{host}Content-Length: 2\r\nContent-Type: application/json\r\n\r\n{{"),
    ] {
        assert_eq!(raw(&server, &text).status, 400, "{text}");
    }
    let huge_header = format!(
        "GET / HTTP/1.1\r\n{host}X-Huge: {}\r\n\r\n",
        "x".repeat(17 * 1024)
    );
    assert_eq!(raw(&server, &huge_header).status, 431);
    let huge_body =
        format!("POST /api/session/open HTTP/1.1\r\n{host}Content-Length: 65537\r\n\r\n");
    assert_eq!(raw(&server, &huge_body).status, 413);
}

#[test]
fn stop_interrupts_incomplete_connections_and_drop_releases_port() {
    let (mut server, _) = start(None);
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, server.local_addr.port())).unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    let before = Instant::now();
    server.stop().unwrap();
    assert!(before.elapsed() < Duration::from_secs(2));
    drop(stream);
    let (server, _) = start(None);
    let port = server.local_addr.port();
    drop(server);
    let _listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
}

#[test]
fn browser_requests_must_be_same_origin_and_use_json() {
    let (server, _) = start(None);
    let host = format!("localhost:{}", server.local_addr.port());
    for headers in [
        format!("Host: attacker.example:{}\r\n", server.local_addr.port()),
        format!("Host: {host}\r\nOrigin: https://evil.example\r\n"),
        format!("Host: {host}\r\nOrigin: null\r\n"),
        format!("Host: {host}\r\nSec-Fetch-Site: cross-site\r\n"),
        format!("Host: {host}\r\nSec-Fetch-Site: same-site\r\n"),
    ] {
        assert_eq!(
            raw(
                &server,
                &format!("POST /api/session/space HTTP/1.1\r\n{headers}Content-Length: 0\r\n\r\n")
            )
            .status,
            403
        );
    }
    assert_eq!(raw(&server, &format!("POST /api/session/open HTTP/1.1\r\nHost: {host}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}")).status, 400);
    assert_eq!(raw(&server, &format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nSec-Fetch-Site: same-origin\r\n\r\n")).status, 200);
}

#[test]
fn library_and_full_study_workflow_persist_progress() {
    let (_dir, library) = fixture();
    let (server, _) = start(Some(library.clone()));
    let reply = request(&server, "GET", "/api/library", None);
    assert_eq!(reply.status, 200);
    let entries = reply.json();
    assert_eq!(entries["library"], library.to_string_lossy().as_ref());
    assert_eq!(
        entries["entries"][0],
        json!({"id":"deck.tsv", "display_name":"deck", "status":"ready", "total":2, "seen":0, "later":0, "opened_at":0})
    );
    let view = post(&server, "/api/session/open", json!({"deck":"deck.tsv"}));
    assert_eq!(view["front"], "First question");
    assert_eq!(view["position"], 1);
    assert_eq!(view["view_total"], 2);
    assert_eq!(view["back"], Value::Null);
    assert_eq!(view["revealed"], false);
    assert_eq!(view["save_blocked"], false);
    let first = view["current_id"].as_str().unwrap().to_owned();
    let view = request(&server, "GET", "/api/session", None).json();
    assert_eq!(view["back"], Value::Null);
    let view = post(&server, "/api/session/space", json!({}));
    assert_eq!(view["revealed"], true);
    assert_eq!(view["back"], "First answer");
    assert_eq!(view["seen"], 1);
    let view = post(&server, "/api/session/space", json!({}));
    assert_eq!(view["front"], "Second question");
    assert_eq!(view["back"], Value::Null);
    assert_eq!(view["position"], 2);
    let second = view["current_id"].as_str().unwrap().to_owned();
    let view = post(&server, "/api/session/toggle-later", json!({}));
    assert_eq!(view["is_later"], true);
    assert_eq!(view["later"], 1);
    // Every mutation is already saved, even before closing.
    let path = progress::state_path(&library.join("deck.tsv"), Some(&library));
    let state = progress::load(&library.join("deck.tsv"), &path).into_state();
    assert!(state.is_seen(&first));
    assert!(state.is_later(&second));
    assert_eq!(
        post(&server, "/api/session/close", json!({})),
        json!({"ok":true})
    );
    assert!(path.starts_with(library.join(".runki")));
    let disk: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(disk["seen"].get(&first).is_some());
    assert_eq!(disk["later"][&second]["on"], true);
    let entries = request(&server, "GET", "/api/library", None).json();
    assert_eq!(entries["entries"][0]["seen"], 1);
    assert_eq!(entries["entries"][0]["later"], 1);
    assert!(entries["entries"][0]["opened_at"].as_i64().unwrap() > 0);
    let view = post(&server, "/api/session/open", json!({"deck":"deck.tsv"}));
    assert_eq!(view["current_id"], second);
    assert_eq!(view["back"], Value::Null);
}

#[test]
fn order_filter_reshuffle_and_reset_use_shared_session_behavior() {
    let (_dir, library) = fixture();
    let (server, _) = start(Some(library));
    post(&server, "/api/session/open", json!({"deck":"deck.tsv"}));
    post(&server, "/api/session/toggle-later", json!({}));
    let view = post(&server, "/api/session/order", json!({"order":"shuffled"}));
    assert_eq!(view["order"], "shuffled");
    let view = post(&server, "/api/session/reshuffle", json!({}));
    assert_eq!(view["order"], "shuffled");
    let view = post(&server, "/api/session/filter", json!({"filter":"later"}));
    assert_eq!(view["filter"], "later");
    assert_eq!(view["view_total"], 1);
    post(&server, "/api/session/space", json!({}));
    let view = post(&server, "/api/session/reset", json!({}));
    assert_eq!(view["seen"], 0);
    assert_eq!(view["later"], 1);
    // Reset retains the revealed answer outside the Unseen filter.
    post(&server, "/api/session/space", json!({}));
    post(&server, "/api/session/order", json!({"order":"deck"}));
    post(&server, "/api/session/filter", json!({"filter":"unseen"}));
    for _ in 0..2 {
        let view = post(&server, "/api/session/space", json!({}));
        assert_eq!(view["empty"], false);
        post(&server, "/api/session/space", json!({}));
    }
    let view = request(&server, "GET", "/api/session", None).json();
    assert_eq!(view["empty"], true);
    assert_eq!(view["position"], Value::Null);
    assert_eq!(view["view_total"], 0);
    let view = post(&server, "/api/session/reset", json!({}));
    assert_eq!(view["empty"], false);
    assert_eq!(view["later"], 1);
    for (path, body) in [
        ("order", json!({"order":"bad"})),
        ("filter", json!({"filter":"bad"})),
    ] {
        assert_eq!(
            request(&server, "POST", &format!("/api/session/{path}"), Some(body)).status,
            400
        );
    }
}

#[test]
fn missing_library_sessions_and_decks_have_clear_errors() {
    let (server, slot) = start(None);
    for (method, path, body, status) in [
        ("GET", "/api/library", None, 409),
        (
            "POST",
            "/api/session/open",
            Some(json!({"deck":"deck.tsv"})),
            409,
        ),
        ("GET", "/api/session", None, 404),
    ] {
        let reply = request(&server, method, path, body);
        assert_eq!(reply.status, status);
        assert!(reply.json()["error"].is_string());
    }
    for action in [
        "space",
        "toggle-later",
        "order",
        "filter",
        "reshuffle",
        "reset",
        "close",
    ] {
        let reply = request(
            &server,
            "POST",
            &format!("/api/session/{action}"),
            Some(json!({})),
        );
        assert_eq!(reply.status, 404);
        assert_eq!(reply.json()["error"], "No deck open");
    }
    let (_dir, library) = fixture();
    *slot.write().unwrap() = Some(library);
    assert_eq!(
        request(&server, "GET", "/api/status", None).json()["has_library"],
        true
    );
    assert_eq!(
        request(&server, "GET", "/api/library", None).json()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        request(
            &server,
            "POST",
            "/api/session/open",
            Some(json!({"deck":"missing.tsv"}))
        )
        .status,
        404
    );
}

#[test]
fn rejects_traversal_and_bad_json_and_never_reads_placeholders() {
    let (_dir, library) = fixture();
    std::fs::write(library.join(".cloud.tsv.icloud"), "NOT A DECK").unwrap();
    std::fs::write(library.join("bad.apkg"), "NOT A DECK").unwrap();
    let (server, _) = start(Some(library));
    for name in [
        "../x.tsv",
        "a/b.tsv",
        "a\\b.tsv",
        "/tmp/x.tsv",
        "..",
        ".",
        "",
        "a\0.tsv",
    ] {
        let reply = request(
            &server,
            "POST",
            "/api/session/open",
            Some(json!({"deck":name})),
        );
        assert_eq!(reply.status, 400, "{name:?}: {}", reply.body);
    }
    for body in [json!({}), json!({"deck":3}), json!(null)] {
        assert_eq!(
            request(&server, "POST", "/api/session/open", Some(body)).status,
            400
        );
    }
    let bad_json = format!("POST /api/session/open HTTP/1.1\r\nHost: localhost:{}\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\n{{", server.local_addr.port());
    assert_eq!(raw(&server, &bad_json).status, 400);
    let entries = request(&server, "GET", "/api/library", None).json();
    let entries = entries["entries"].as_array().unwrap();
    assert_eq!(
        entries.iter().find(|e| e["id"] == "cloud.tsv").unwrap()["status"],
        "not_downloaded"
    );
    assert_eq!(
        entries.iter().find(|e| e["id"] == "bad.apkg").unwrap()["status"],
        "failed"
    );
    assert_eq!(
        request(
            &server,
            "POST",
            "/api/session/open",
            Some(json!({"deck":"cloud.tsv"}))
        )
        .status,
        409
    );
    assert_eq!(
        request(
            &server,
            "POST",
            "/api/session/open",
            Some(json!({"deck":"bad.apkg"}))
        )
        .status,
        500
    );
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_the_library() {
    let (_dir, library) = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.tsv"), "secret\tanswer\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.tsv"),
        library.join("escape.tsv"),
    )
    .unwrap();
    let (server, _) = start(Some(library));
    assert_eq!(
        request(
            &server,
            "POST",
            "/api/session/open",
            Some(json!({"deck":"escape.tsv"}))
        )
        .status,
        400
    );
    let entries = request(&server, "GET", "/api/library", None).json();
    let entry = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "escape.tsv")
        .unwrap();
    assert_eq!(entry["status"], "failed");
}

#[test]
fn unreadable_progress_stays_usable_in_memory_and_is_never_overwritten() {
    let (_dir, library) = fixture();
    let path = progress::state_path(&library.join("deck.tsv"), Some(&library));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "broken progress").unwrap();
    let (mut server, _) = start(Some(library));
    assert_eq!(
        post(&server, "/api/session/open", json!({"deck":"deck.tsv"}))["save_blocked"],
        true
    );
    assert_eq!(post(&server, "/api/session/space", json!({}))["seen"], 1);
    assert_eq!(
        post(&server, "/api/session/toggle-later", json!({}))["later"],
        1
    );
    post(&server, "/api/session/close", json!({}));
    server.stop().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "broken progress");
}

#[test]
fn save_errors_keep_outgoing_session_for_retry() {
    let (_dir, library) = fixture();
    std::fs::write(library.join("second.tsv"), "other\tanswer\n").unwrap();
    let (server, _) = start(Some(library.clone()));
    post(&server, "/api/session/open", json!({"deck":"deck.tsv"}));
    let path = progress::state_path(&library.join("deck.tsv"), Some(&library));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        request(&server, "POST", "/api/session/space", Some(json!({}))).status,
        500
    );
    for (endpoint, body) in [("open", json!({"deck":"second.tsv"})), ("close", json!({}))] {
        assert_eq!(
            request(
                &server,
                "POST",
                &format!("/api/session/{endpoint}"),
                Some(body)
            )
            .status,
            500
        );
        assert_eq!(
            request(&server, "GET", "/api/session", None).json()["file_name"],
            "deck.tsv"
        );
    }
    std::fs::remove_dir(path).unwrap();
    post(&server, "/api/session/close", json!({}));
    let state = progress::load(
        &library.join("deck.tsv"),
        &progress::state_path(&library.join("deck.tsv"), Some(&library)),
    )
    .into_state();
    assert_eq!(state.seen.len(), 1);
}

#[test]
fn disk_updates_merge_with_web_actions_and_deck_switch_saves() {
    let (_dir, library) = fixture();
    std::fs::write(library.join("second.tsv"), "other\tanswer\n").unwrap();
    let (mut server, _) = start(Some(library.clone()));
    let view = post(&server, "/api/session/open", json!({"deck":"deck.tsv"}));
    let first = view["current_id"].as_str().unwrap();
    let path = progress::state_path(&library.join("deck.tsv"), Some(&library));
    let mut state = progress::load(&library.join("deck.tsv"), &path).into_state();
    state.set_later(first, true, progress::now_ms());
    progress::save(&path, &state).unwrap();
    let view = post(&server, "/api/session/space", json!({}));
    assert_eq!(view["is_later"], true);
    post(&server, "/api/session/open", json!({"deck":"second.tsv"}));
    server.stop().unwrap();
    let state = progress::load(&library.join("deck.tsv"), &path).into_state();
    assert!(state.is_seen(first));
    assert!(state.is_later(first));
    let path = progress::state_path(&library.join("second.tsv"), Some(&library));
    assert!(path.exists());
}
