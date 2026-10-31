//! Per-deck progress: study-later flags, seen marks, and view prefs.
//!
//! State lives in a small JSON file so it can sync between machines
//! next to the deck: a `<deck>.runki.json` sidecar, or
//! `<library>/.runki/<deck file name>.json` for library decks.
//! All times are unix milliseconds. Merging is last-writer-wins per
//! entry, so two machines never lose each other's changes as long as
//! one machine's file has synced before the other saves.
//!
//! This module has no egui dependency; logic methods take `now` as a
//! parameter so they are easy to test.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// The only state-file version this build can read and write.
pub const CURRENT_VERSION: u32 = 1;

fn current_version() -> u32 {
    CURRENT_VERSION
}

/// Current wall-clock time in unix milliseconds.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(err) => -i64::try_from(err.duration().as_millis()).unwrap_or(i64::MIN),
    }
}

/// Card order in the study view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardOrder {
    #[default]
    Deck,
    Shuffled,
}

/// Which cards the study view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardFilter {
    #[default]
    All,
    Unseen,
    Later,
}

/// One study-later flag: a last-writer-wins register.
/// Unflagging writes `on: false` with a newer `t`; it is not deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaterEntry {
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub t: i64,
}

/// Order/filter/seed/current card as one last-writer-wins unit
/// (`t` = when any of them last changed).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub t: i64,
    #[serde(default)]
    pub order: CardOrder,
    #[serde(default)]
    pub filter: CardFilter,
    #[serde(default)]
    pub shuffle_seed: u64,
    /// Current card id, so it survives reshuffles and deck edits.
    #[serde(default)]
    pub position: Option<String>,
}

/// Progress for one deck, as stored in its state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckState {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub later: BTreeMap<String, LaterEntry>,
    /// Card id -> unix ms it was last seen. A card is seen iff its
    /// time is `> reset_at`.
    #[serde(default)]
    pub seen: BTreeMap<String, i64>,
    #[serde(default)]
    pub reset_at: i64,
    #[serde(default)]
    pub prefs: Prefs,
    /// Last time the deck was opened on any machine.
    #[serde(default)]
    pub opened_at: i64,
}

impl Default for DeckState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            later: BTreeMap::new(),
            seen: BTreeMap::new(),
            reset_at: 0,
            prefs: Prefs::default(),
            opened_at: 0,
        }
    }
}

impl DeckState {
    /// An empty state (no flags, nothing seen).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a card is flagged for later.
    pub fn is_later(&self, id: &str) -> bool {
        self.later.get(id).is_some_and(|entry| entry.on)
    }

    /// Flag or unflag a card at `now`.
    pub fn set_later(&mut self, id: &str, on: bool, now: i64) {
        self.later.insert(id.to_string(), LaterEntry { on, t: now });
    }

    /// Whether a card has been seen since the last reset.
    /// A time exactly equal to `reset_at` is not seen.
    pub fn is_seen(&self, id: &str) -> bool {
        self.seen.get(id).is_some_and(|&t| t > self.reset_at)
    }

    /// Record that a card was seen at `now`.
    pub fn mark_seen(&mut self, id: &str, now: i64) {
        let entry = self.seen.entry(id.to_string()).or_insert(now);
        *entry = (*entry).max(now);
    }

    /// Clear seen marks (study-later flags are kept).
    /// Existing `seen` times are left alone; they simply stop
    /// counting because they are no longer `> reset_at`.
    pub fn reset(&mut self, now: i64) {
        self.reset_at = now;
    }

    /// Record that the deck was opened at `now`.
    pub fn mark_opened(&mut self, now: i64) {
        self.opened_at = self.opened_at.max(now);
    }

    /// Merge two states. `later` per id: larger `t` wins (tie ->
    /// `on: true`); `seen` per id: max; `reset_at`: max; `prefs`:
    /// larger `t` (tie -> `self`); `opened_at`: max.
    ///
    /// Merge is commutative, associative, and idempotent except for
    /// the documented `prefs` tie rule.
    pub fn merge(&self, other: &DeckState) -> DeckState {
        let mut later = self.later.clone();
        for (id, entry) in &other.later {
            match later.get(id) {
                None => {
                    later.insert(id.clone(), *entry);
                }
                Some(existing) if entry.t > existing.t => {
                    later.insert(id.clone(), *entry);
                }
                Some(existing) if entry.t == existing.t && entry.on && !existing.on => {
                    later.insert(id.clone(), *entry);
                }
                _ => {}
            }
        }
        let mut seen = self.seen.clone();
        for (id, &t) in &other.seen {
            let entry = seen.entry(id.clone()).or_insert(t);
            *entry = (*entry).max(t);
        }
        DeckState {
            version: CURRENT_VERSION,
            later,
            seen,
            reset_at: self.reset_at.max(other.reset_at),
            prefs: if other.prefs.t > self.prefs.t {
                other.prefs.clone()
            } else {
                self.prefs.clone()
            },
            opened_at: self.opened_at.max(other.opened_at),
        }
    }

    /// Number of the given card ids that are flagged for later.
    /// Ids not in `ids` (stale ids kept in the file) are not counted.
    pub fn later_count<'a, I>(&self, ids: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        ids.into_iter().filter(|id| self.is_later(id)).count()
    }

    /// Number of the given card ids that have been seen.
    pub fn seen_count<'a, I>(&self, ids: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        ids.into_iter().filter(|id| self.is_seen(id)).count()
    }
}

/// Where a loaded state came from, or why saving must be blocked.
#[derive(Debug)]
pub enum LoadOutcome {
    /// State file read fine, or there was nothing to read yet.
    Ok(DeckState),
    /// No state file, but a legacy `<deck>.later.json` was imported.
    Migrated(DeckState),
    /// The state (or legacy) file could not be read. The in-memory
    /// state is empty and saving must be blocked: never overwrite a
    /// file we cannot read.
    Blocked {
        state: DeckState,
        error: anyhow::Error,
    },
}

impl LoadOutcome {
    /// The loaded (or, when blocked, empty) state.
    pub fn state(&self) -> &DeckState {
        match self {
            LoadOutcome::Ok(state) | LoadOutcome::Migrated(state) => state,
            LoadOutcome::Blocked { state, .. } => state,
        }
    }

    /// Move the state out of the outcome.
    pub fn into_state(self) -> DeckState {
        match self {
            LoadOutcome::Ok(state) | LoadOutcome::Migrated(state) => state,
            LoadOutcome::Blocked { state, .. } => state,
        }
    }

    /// Whether saving must be blocked for this deck.
    pub fn is_blocked(&self) -> bool {
        matches!(self, LoadOutcome::Blocked { .. })
    }

    /// Whether the state was imported from a legacy file.
    pub fn is_migrated(&self) -> bool {
        matches!(self, LoadOutcome::Migrated(_))
    }

    /// The error that blocked saving, if any.
    pub fn error(&self) -> Option<&anyhow::Error> {
        match self {
            LoadOutcome::Blocked { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Resolve the state-file path for a deck.
///
/// If `library` is given and the deck's canonical parent directory is
/// the canonical library directory, the state lives in
/// `<library>/.runki/<deck file name>.json`. Otherwise it is a
/// sidecar `<deck file name>.runki.json` next to the deck.
pub fn state_path(deck_path: &Path, library: Option<&Path>) -> PathBuf {
    if let Some(library) = library {
        if let Some(path) = library_state_path(deck_path, library) {
            return path;
        }
    }
    sidecar_path(deck_path, ".runki.json")
}

/// `<library>/.runki/<deck file name>.json` if the deck sits
/// directly in the library, else `None`.
fn library_state_path(deck_path: &Path, library: &Path) -> Option<PathBuf> {
    let parent = deck_path.parent().filter(|p| !p.as_os_str().is_empty());
    let canonical_parent = parent
        .map_or_else(|| std::fs::canonicalize("."), std::fs::canonicalize)
        .ok()?;
    let canonical_library = std::fs::canonicalize(library).ok()?;
    if canonical_parent != canonical_library {
        return None;
    }
    let mut file_name: OsString = deck_path.file_name()?.to_owned();
    file_name.push(".json");
    Some(library.join(".runki").join(file_name))
}

/// `deck_path` with `suffix` appended to the whole file name.
/// Appends to the `OsString` so non-UTF-8 paths are kept exactly.
fn sidecar_path(deck_path: &Path, suffix: &str) -> PathBuf {
    let mut os = deck_path.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// Path of the legacy `<deck>.later.json` study-later file.
fn legacy_later_path(deck_path: &Path) -> PathBuf {
    sidecar_path(deck_path, ".later.json")
}

/// Load the state for a deck from `state_path`.
///
/// A missing file gives an empty state (after trying migration from
/// the legacy file next to `deck_path`). An unreadable file, invalid
/// JSON, or a `version` newer than this build understands gives
/// [`LoadOutcome::Blocked`].
pub fn load(deck_path: &Path, state_path: &Path) -> LoadOutcome {
    match read_state_file(state_path) {
        Ok(Some(state)) => LoadOutcome::Ok(state),
        Ok(None) => load_legacy(deck_path),
        Err(error) => LoadOutcome::Blocked {
            state: DeckState::new(),
            error,
        },
    }
}

/// Import a legacy `<deck>.later.json` if there is one.
/// The old file is kept. An unreadable legacy file blocks saving.
fn load_legacy(deck_path: &Path) -> LoadOutcome {
    let path = legacy_later_path(deck_path);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return LoadOutcome::Ok(DeckState::new());
        }
        Err(err) => {
            return LoadOutcome::Blocked {
                state: DeckState::new(),
                error: anyhow::Error::from(err).context(format!(
                    "Could not read study-later list in {}",
                    path.display()
                )),
            };
        }
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(ids) => {
            let mut state = DeckState::new();
            for id in ids {
                state.later.insert(id, LaterEntry { on: true, t: 0 });
            }
            LoadOutcome::Migrated(state)
        }
        Err(err) => LoadOutcome::Blocked {
            state: DeckState::new(),
            error: anyhow::Error::from(err).context(format!(
                "Could not parse study-later list in {}",
                path.display()
            )),
        },
    }
}

/// Read and parse a state file: `None` if it does not exist yet,
/// `Err` if it is unreadable, invalid, or from a newer version.
/// Crate-visible so the library can parse sync conflict copies
/// without going through deck-oriented [`load`] (which would try
/// legacy migration for a missing file).
pub(crate) fn read_state_file(path: &Path) -> Result<Option<DeckState>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Could not read progress in {}", path.display()));
        }
    };
    parse_state(&text, path).map(Some)
}

/// Parse state-file JSON, rejecting versions newer than this build.
fn parse_state(text: &str, path: &Path) -> Result<DeckState> {
    // Check the version before deserializing, so a future format that
    // changed field shapes is reported as "newer", not "invalid".
    let value: serde_json::Value = serde_json::from_str(text)
        .with_context(|| format!("Could not parse progress in {}", path.display()))?;
    if let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) {
        if version > u64::from(CURRENT_VERSION) {
            return Err(anyhow!(
                "Progress in {} was written by a newer version of Runki \
                 (version {version}, this build understands up to {CURRENT_VERSION})",
                path.display()
            ));
        }
    }
    let mut state: DeckState = serde_json::from_value(value)
        .with_context(|| format!("Could not parse progress in {}", path.display()))?;
    state.version = CURRENT_VERSION;
    Ok(state)
}

/// Save `state` to `state_path`, merging with what is on disk.
///
/// The file is re-read first; if it is unreadable, invalid, or from a
/// newer version, nothing is written and an error is returned.
/// Otherwise `merge(state, disk)` is written atomically (hidden temp
/// file in the same directory, `sync_all`, rename) and the merged
/// state is returned so the caller can adopt it.
pub fn save(state_path: &Path, state: &DeckState) -> Result<DeckState> {
    let disk = read_state_file(state_path)?.unwrap_or_default();
    let merged = state.merge(&disk);
    let text = serde_json::to_string_pretty(&merged).context("Could not serialize progress")?;
    write_atomically(state_path, text.as_bytes())?;
    Ok(merged)
}

/// Write `bytes` to `path` via a hidden temp file in the same
/// directory, so a crash or a sync client never sees a partial file.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    let tmp_path = temp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result.with_context(|| format!("Could not save progress to {}", path.display()))?;
    Ok(())
}

/// Hidden temp path `.<name>.<pid>.tmp` next to `path`.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    if let Some(file_name) = path.file_name() {
        name.push(file_name);
    }
    name.push(format!(".{}.tmp", std::process::id()));
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}
