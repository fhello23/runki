//! Optional LAN study interface. Only the listener and connection workers use
//! threads; progress and study behavior are shared with the native interfaces.

mod http;

use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::deck;
use crate::library::{self, DeckStatus};
use crate::progress::{self, CardFilter, CardOrder};
use crate::session::StudySession;

pub type LibrarySlot = Arc<RwLock<Option<PathBuf>>>;

/// CLI overrides are effective only for this run. Frontends persist deliberate
/// toggle changes, without copying these values into the remembered settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WebOptions {
    pub on: Option<bool>,
    pub port: Option<u16>,
}

pub const WARNING: &str = "Anyone on this network can study and change progress.";

pub struct WebServer {
    pub local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<ServerState>>,
    lan_url: String,
}

impl WebServer {
    /// Port zero is supported for tests and selects an ephemeral port.
    pub fn start(library: LibrarySlot, port: u16) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .with_context(|| format!("Could not start Web on port {port}"))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(ServerState { session: None }));
        let stop = Arc::clone(&shutdown);
        let shared = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("runki-web".into())
            .spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let mut i = 0;
                    while i < workers.len() {
                        if workers[i].is_finished() {
                            let _ = workers.swap_remove(i).join();
                        } else {
                            i += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Bound resource use even when clients leave requests unfinished.
                            if workers.len() >= 32 {
                                drop(stream);
                                continue;
                            }
                            let shared = Arc::clone(&shared);
                            let library = Arc::clone(&library);
                            let stop = Arc::clone(&stop);
                            if let Ok(worker) = thread::Builder::new()
                                .name("runki-http".into())
                                .spawn(move || {
                                    http::serve(stream, &stop, local_addr.port(), |request| {
                                        route(request, &library, &shared)
                                    });
                                })
                            {
                                workers.push(worker);
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
                // Close the listener before waiting for in-flight requests. Workers
                // observe shutdown during reads and have bounded write timeouts.
                drop(listener);
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Self {
            local_addr,
            shutdown,
            thread: Some(thread),
            state,
            lan_url: format!("http://{}:{}/", lan_ip(), local_addr.port()),
        })
    }

    pub fn lan_url(&self) -> &str {
        &self.lan_url
    }

    pub fn localhost_url(&self) -> String {
        format!("http://localhost:{}/", self.local_addr.port())
    }

    /// Stop accepting, finish workers, and flush the web session. Safe to repeat;
    /// a failed flush can be retried while the handle remains alive.
    pub fn stop(&mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("Web server thread panicked"))?;
        }
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("Web session lock poisoned"))?
            .save()
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// UDP connect chooses a route without sending a packet.
pub fn lan_ip() -> Ipv4Addr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80))?;
            socket.local_addr()
        })
        .ok()
        .and_then(|addr| match addr.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

struct ServerState {
    session: Option<StudySession>,
}

impl ServerState {
    fn save(&mut self) -> Result<()> {
        if let Some(session) = &mut self.session {
            session.save().with_context(|| {
                format!("Progress for {} could not be saved", session.deck().name)
            })?;
        }
        Ok(())
    }
}

type ApiResult = std::result::Result<Value, http::Response>;

fn io_error(error: impl std::fmt::Display) -> http::Response {
    http::Response::error(500, format!("{error:#}"))
}

fn library_dir(library: &LibrarySlot) -> std::result::Result<PathBuf, http::Response> {
    library
        .read()
        .map_err(|_| io_error("Library lock poisoned"))?
        .clone()
        .ok_or_else(|| http::Response::error(409, "No library folder chosen in Runki"))
}

fn route(
    request: http::Request,
    library: &LibrarySlot,
    state: &Mutex<ServerState>,
) -> http::Response {
    let expected = match request.path.as_str() {
        "/" | "/api/status" | "/api/library" | "/api/session" => "GET",
        "/api/session/open"
        | "/api/session/space"
        | "/api/session/toggle-later"
        | "/api/session/order"
        | "/api/session/filter"
        | "/api/session/reshuffle"
        | "/api/session/reset"
        | "/api/session/close" => "POST",
        _ => return http::Response::error(404, "Unknown route"),
    };
    if request.method != expected {
        return http::Response::error(405, "Method not allowed");
    }
    if request.path == "/" {
        return http::Response::html(include_str!("index.html"));
    }
    let result = (|| -> ApiResult {
        if request.path == "/api/library" {
            return library_view(&library_dir(library)?);
        }
        let mut state = state
            .lock()
            .map_err(|_| io_error("Web session lock poisoned"))?;
        if request.path == "/api/status" {
            return Ok(json!({"version": crate::VERSION,
                "has_library": library.read().map_err(|_| io_error("Library lock poisoned"))?.is_some(),
                "deck_open": state.session.is_some()}));
        }
        if request.path == "/api/session/open" {
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|_| http::Response::error(400, "Expected JSON with a deck file name"))?;
            let name = body
                .get("deck")
                .and_then(Value::as_str)
                .ok_or_else(|| http::Response::error(400, "Expected a deck file name"))?;
            if name.is_empty()
                || name.contains(['/', '\\', '\0'])
                || name.contains("..")
                || std::path::Path::new(name).is_absolute()
                || std::path::Path::new(name)
                    .file_name()
                    .and_then(|s| s.to_str())
                    != Some(name)
            {
                return Err(http::Response::error(400, "Deck must be a plain file name"));
            }
            let dir = library_dir(library)?;
            let entries = library::scan(&dir).map_err(io_error)?;
            let entry = entries
                .iter()
                .find(|entry| entry.path.file_name().and_then(|s| s.to_str()) == Some(name))
                .ok_or_else(|| http::Response::error(404, "Deck not found"))?;
            if entry.status == DeckStatus::NotDownloaded {
                return Err(http::Response::error(
                    409,
                    "Deck is not downloaded. Download it in Runki's library folder first.",
                ));
            }
            let canonical_dir = std::fs::canonicalize(&dir).map_err(io_error)?;
            let path = std::fs::canonicalize(&entry.path).map_err(io_error)?;
            if path.parent() != Some(canonical_dir.as_path()) || !library::is_deck_path(&path) {
                return Err(http::Response::error(
                    400,
                    "Deck must be inside the library folder",
                ));
            }
            // Keep the listed filename as the progress key, like native clients.
            let path = canonical_dir.join(name);
            state.save().map_err(io_error)?;
            let deck = deck::load_deck(&path).map_err(io_error)?;
            let state_path = progress::state_path(&path, Some(&canonical_dir));
            let outcome = load_progress(&path, &state_path);
            let blocked = outcome.is_blocked();
            state.session = Some(StudySession::new(
                deck,
                outcome.into_state(),
                state_path,
                blocked,
                progress::now_ms(),
            ));
            state.save().map_err(io_error)?;
            return Ok(session_view(state.session.as_ref().unwrap()));
        }
        if request.path == "/api/session/close" {
            if state.session.is_none() {
                return Err(http::Response::error(404, "No deck open"));
            }
            state.save().map_err(io_error)?;
            state.session = None;
            return Ok(json!({"ok": true}));
        }
        let session = state
            .session
            .as_mut()
            .ok_or_else(|| http::Response::error(404, "No deck open"))?;
        if request.method == "POST" {
            let now = progress::now_ms();
            match request.path.as_str() {
                "/api/session/space" => session.space(now),
                "/api/session/toggle-later" => session.toggle_later(now),
                "/api/session/reshuffle" => session.reshuffle(now),
                "/api/session/reset" => session.reset(now),
                "/api/session/order" | "/api/session/filter" => {
                    let body: Value = serde_json::from_slice(&request.body)
                        .map_err(|_| http::Response::error(400, "Expected JSON"))?;
                    if request.path.ends_with("/order") {
                        let order = match body.get("order").and_then(Value::as_str) {
                            Some("deck") => CardOrder::Deck,
                            Some("shuffled") => CardOrder::Shuffled,
                            _ => {
                                return Err(http::Response::error(
                                    400,
                                    "Order must be deck or shuffled",
                                ))
                            }
                        };
                        session.set_order(order, now);
                    } else {
                        let filter = match body.get("filter").and_then(Value::as_str) {
                            Some("all") => CardFilter::All,
                            Some("unseen") => CardFilter::Unseen,
                            Some("later") => CardFilter::Later,
                            _ => {
                                return Err(http::Response::error(
                                    400,
                                    "Filter must be all, unseen or later",
                                ))
                            }
                        };
                        session.set_filter(filter, now);
                    }
                }
                _ => unreachable!(),
            }
            session.save().map_err(io_error)?;
        }
        Ok(session_view(session))
    })();
    match result {
        Ok(value) => http::Response::json(200, value),
        Err(response) => response,
    }
}

fn library_view(dir: &std::path::Path) -> ApiResult {
    let mut entries = library::scan(dir).map_err(io_error)?;
    let canonical_dir = std::fs::canonicalize(dir).map_err(io_error)?;
    for entry in &mut entries {
        if entry.status == DeckStatus::NotDownloaded {
            continue;
        }
        // Do not follow a symlink out of the library, even for summaries.
        let inside = std::fs::canonicalize(&entry.path).is_ok_and(|path| {
            path.parent() == Some(canonical_dir.as_path()) && library::is_deck_path(&path)
        });
        if !inside {
            entry.status = DeckStatus::Failed("Deck must be inside the library folder".into());
            continue;
        }
        let state_path = progress::state_path(&entry.path, Some(dir));
        let state = load_progress(&entry.path, &state_path).into_state();
        entry.opened_at = state.opened_at;
        entry.status = match deck::load_deck(&entry.path) {
            Ok(deck) => DeckStatus::Ready {
                total: deck.cards.len(),
                seen: state.seen_count(deck.cards.iter().map(|card| card.id.as_str())),
                later: state.later_count(deck.cards.iter().map(|card| card.id.as_str())),
            },
            Err(err) => DeckStatus::Failed(format!("{err:#}")),
        };
    }
    library::sort_entries(&mut entries);
    let entries: Vec<Value> = entries.into_iter().map(|entry| {
        let mut value = json!({"id": entry.path.file_name().unwrap_or_default().to_string_lossy(),
            "display_name": entry.display_name, "opened_at": entry.opened_at});
        match entry.status {
            DeckStatus::Ready {total, seen, later} => {
                value["status"] = json!("ready");
                value["total"] = json!(total);
                value["seen"] = json!(seen);
                value["later"] = json!(later);
            }
            DeckStatus::Failed(error) => {
                value["status"] = json!("failed");
                value["error"] = json!(error);
            }
            DeckStatus::NotDownloaded => value["status"] = json!("not_downloaded"),
            DeckStatus::Loading => unreachable!(),
        }
        value
    }).collect();
    Ok(json!({"library": dir.to_string_lossy(), "entries": entries}))
}

fn session_view(session: &StudySession) -> Value {
    let card = session.current_card();
    let position = session.view_position();
    json!({
        "deck_name": session.deck().name,
        "file_name": session.deck().path.file_name().unwrap_or_default().to_string_lossy(),
        "revealed": session.revealed(),
        "front": card.map(|card| card.front.as_str()),
        "back": card.filter(|_| session.revealed()).map(|card| card.back.as_str()),
        "current_id": session.current_id(),
        "is_later": session.current_id().is_some_and(|id| session.is_later(id)),
        "is_seen": session.current_id().is_some_and(|id| session.is_seen(id)),
        "position": position.map(|(n, _)| n),
        "view_total": position.map_or(0, |(_, n)| n),
        "total": session.total(), "seen": session.seen_count(), "later": session.later_count(),
        "order": match session.card_order() { CardOrder::Deck => "deck", CardOrder::Shuffled => "shuffled" },
        "filter": match session.filter() { CardFilter::All => "all", CardFilter::Unseen => "unseen", CardFilter::Later => "later" },
        "empty": card.is_none(), "save_blocked": session.save_blocked(),
    })
}

/// Like the native library loader, never hydrate online-only progress files.
fn load_progress(path: &std::path::Path, state_path: &std::path::Path) -> progress::LoadOutcome {
    let mut legacy = path.as_os_str().to_owned();
    legacy.push(".later.json");
    if library::is_dataless(state_path)
        || (!state_path.exists() && library::is_dataless(std::path::Path::new(&legacy)))
    {
        return progress::LoadOutcome::Blocked {
            state: progress::DeckState::new(),
            error: anyhow::anyhow!("Progress is not downloaded"),
        };
    }
    progress::load(path, state_path)
}
