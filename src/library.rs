//! The deck library: one folder of deck files, shown as a list.
//!
//! The folder is meant to sync between machines (Dropbox, iCloud
//! Drive, or any local folder); per-deck progress lives in its
//! `.runki/` subfolder (see [`crate::progress`]). This module scans
//! the folder, copies new decks in ([`add_deck`]), and loads card
//! counts in the background ([`LibraryLoader`]) so the list appears
//! at once and fills in. It has no egui dependency: the loader's
//! repaint hook is a plain callback the app wires to
//! `ctx.request_repaint()`.
//!
//! Sync hardening for cloud folders lives here too: online-only
//! (dataless / placeholder) decks are listed as
//! [`DeckStatus::NotDownloaded`] and never read, and sync conflict
//! copies of state files in `.runki/` are merged into the real
//! state and removed by the background loader (see
//! `merge_conflict_copies`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::deck::{self, Deck};
use crate::progress;

/// Deck file extensions the library lists (compared lower-cased).
pub const DECK_EXTENSIONS: &[&str] = &["apkg", "csv", "tsv", "txt"];

/// Load state of one library row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckStatus {
    /// The background loader has not reported on this deck yet.
    Loading,
    /// The deck parsed; counts only include ids present in the deck.
    Ready {
        total: usize,
        seen: usize,
        later: usize,
    },
    /// The deck could not be loaded; the row stays listed, unclickable.
    Failed(String),
    /// The file is an online-only placeholder (macOS dataless, or
    /// a legacy `.<name>.icloud` placeholder). Never read by the
    /// background loader; the row is not clickable.
    NotDownloaded,
}

/// One row of the library list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckEntry {
    pub path: PathBuf,
    /// File stem, shown as the deck's name.
    pub display_name: String,
    pub status: DeckStatus,
    /// Unix ms the deck was last opened (from its state file), or 0.
    pub opened_at: i64,
}

/// Whether `path` has a deck extension (case-insensitive).
pub fn is_deck_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| DECK_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Whether a file name should never be listed as a deck: dotfiles,
/// progress sidecars, and temp files. (The extension check already
/// excludes the sidecars; this keeps e.g. `deck.tmp.csv`-style
/// stragglers and hidden working files out too.)
fn is_hidden_or_sidecar(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.starts_with('.')
        || lower.ends_with(".later.json")
        || lower.ends_with(".runki.json")
        || lower.ends_with(".tmp")
}

/// Whether `path` is a dataless (online-only) placeholder: the file
/// is listed in the folder but its contents are not on this Mac, so
/// opening it would block on a download. On macOS this is the
/// `SF_DATALESS` flag in the file's metadata; the check only stats
/// the file (`metadata`), it never opens or reads it. Everywhere
/// else (and for a missing/unreadable path) it is `false`.
#[cfg(target_os = "macos")]
pub fn is_dataless(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    /// `SF_DATALESS` from `<sys/stat.h>`.
    const SF_DATALESS: u32 = 0x4000_0000;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.st_flags() & SF_DATALESS != 0)
}

/// Non-macOS fallback: this build cannot detect dataless files.
#[cfg(not(target_os = "macos"))]
pub fn is_dataless(_path: &Path) -> bool {
    false
}

/// The real file name behind a legacy iCloud placeholder name:
/// `.<name>.icloud` -> `<name>` (e.g. `.French.apkg.icloud` ->
/// `French.apkg`). `None` if `file_name` is not such a placeholder.
fn icloud_placeholder_real_name(file_name: &str) -> Option<String> {
    let real_name = file_name.strip_prefix('.')?.strip_suffix(".icloud")?;
    (!real_name.is_empty()).then(|| real_name.to_string())
}

/// Path of the legacy iCloud placeholder for a real deck path:
/// `French.apkg` -> `.French.apkg.icloud`, in the same folder.
pub fn icloud_placeholder_path(real_path: &Path) -> PathBuf {
    let Some(file_name) = real_path.file_name().and_then(|name| name.to_str()) else {
        return real_path.to_path_buf();
    };
    real_path.with_file_name(format!(".{file_name}.icloud"))
}

/// The path to reveal for a deck row (e.g. with `open -R` in
/// Finder): the real file if it exists, otherwise its iCloud
/// placeholder if that exists, otherwise the real path anyway.
/// Only stats the paths; never opens them.
pub fn reveal_path(real_path: &Path) -> PathBuf {
    if real_path.exists() {
        return real_path.to_path_buf();
    }
    let placeholder = icloud_placeholder_path(real_path);
    if placeholder.exists() {
        return placeholder;
    }
    real_path.to_path_buf()
}

/// Whether a deck must not be read because only a placeholder is
/// present: it is dataless, or the real file is missing while its
/// legacy iCloud placeholder exists.
fn is_not_downloaded(path: &Path) -> bool {
    is_dataless(path) || (!path.exists() && icloud_placeholder_path(path).exists())
}

/// Paths the background loader may read: every row except
/// [`DeckStatus::NotDownloaded`] ones, which are never sent to it.
pub fn loadable_paths(entries: &[DeckEntry]) -> Vec<PathBuf> {
    entries
        .iter()
        .filter(|entry| entry.status != DeckStatus::NotDownloaded)
        .map(|entry| entry.path.clone())
        .collect()
}

/// List the decks directly in `dir`, sorted by last opened (newest
/// first), then name (case-insensitive).
///
/// Rows come back as [`DeckStatus::Loading`] with `opened_at` 0:
/// scan only lists and stats directory entries, it never opens a
/// deck or a state file (either could be an online-only file whose
/// read would block the caller — the UI thread — on a download).
/// The background loader reads each deck's state and re-sorts the
/// rows as `opened_at` arrives in its summaries. Placeholder decks
/// (dataless, or a legacy `.<name>.icloud` file) come back as
/// [`DeckStatus::NotDownloaded`] instead. A missing or unreadable
/// folder is an `Err` with a message naming the folder.
pub fn scan(dir: &Path) -> Result<Vec<DeckEntry>> {
    let read_dir = std::fs::read_dir(dir)
        .with_context(|| format!("Could not read library folder {}", dir.display()))?;
    let mut files = Vec::new();
    for entry in read_dir {
        let entry =
            entry.with_context(|| format!("Could not read library folder {}", dir.display()))?;
        let path = entry.path();
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            files.push(path);
        }
    }
    let file_names: HashSet<String> = files
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect();

    let mut entries = Vec::new();
    for path in files {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // A legacy iCloud placeholder lists its real deck, unless
        // the real file is also here (then it is listed below).
        if let Some(real_name) = icloud_placeholder_real_name(file_name) {
            let real_path = dir.join(&real_name);
            if file_names.contains(real_name.as_str())
                || is_hidden_or_sidecar(&real_name)
                || !is_deck_path(&real_path)
            {
                continue;
            }
            let display_name = real_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(&real_name)
                .to_string();
            entries.push(DeckEntry {
                path: real_path,
                display_name,
                status: DeckStatus::NotDownloaded,
                opened_at: 0,
            });
            continue;
        }
        if is_hidden_or_sidecar(file_name) || !is_deck_path(&path) {
            continue;
        }
        let display_name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(file_name)
            .to_string();
        let status = if is_dataless(&path) {
            DeckStatus::NotDownloaded
        } else {
            DeckStatus::Loading
        };
        entries.push(DeckEntry {
            path,
            display_name,
            status,
            opened_at: 0,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Library order: last opened first, then name (case-insensitive).
/// Re-applied when background summaries update `opened_at`.
pub fn sort_entries(entries: &mut [DeckEntry]) {
    entries.sort_by(|a, b| {
        b.opened_at
            .cmp(&a.opened_at)
            .then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Human label for a last-opened time: "never", "today",
/// "yesterday", or "N days ago". Days are whole UTC days apart, so
/// near midnight it can be off by one for some time zones; fine for
/// a list label.
pub fn last_opened_label(opened_at: i64, now: i64) -> String {
    if opened_at <= 0 {
        return "never".to_string();
    }
    const DAY_MS: i64 = 86_400_000;
    let days = now.div_euclid(DAY_MS) - opened_at.div_euclid(DAY_MS);
    match days {
        i64::MIN..=0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

/// Whether the deck at `source` already sits directly in `library`
/// (canonical parent directories match), so no copy is needed.
fn is_in_library(source: &Path, library: &Path) -> bool {
    let Some(parent) = source.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return false;
    };
    let Ok(canonical_parent) = std::fs::canonicalize(parent) else {
        return false;
    };
    std::fs::canonicalize(library).is_ok_and(|lib| lib == canonical_parent)
}

/// The `n`th candidate file name in `library` for a deck called
/// `file_name`: the name itself, then `name (2).ext`, `name (3).ext`, …
fn candidate_name(file_name: &std::ffi::OsStr, n: usize) -> OsString {
    if n <= 1 {
        return file_name.to_owned();
    }
    let path = Path::new(file_name);
    let mut name = path
        .file_stem()
        .map_or_else(|| file_name.to_owned(), std::ffi::OsStr::to_owned);
    name.push(format!(" ({n})"));
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    name
}

/// Copy `source` into the library and return its new path.
///
/// A name clash picks `name (2).ext`, `name (3).ext`, … — an
/// existing file is never overwritten. The copy goes to a hidden
/// temp file `.<name>.<pid>.tmp` in the library, is synced, then
/// renamed (re-checking the target is still free right before), so
/// sync clients and the scanner never see a partial file.
///
/// Progress for the source deck — its `.runki.json` sidecar, or a
/// legacy `.later.json` migrated by [`progress::load`] — is carried
/// over into the new deck's library state file. A source already
/// directly in the library is not copied; its path is returned.
pub fn add_deck(library: &Path, source: &Path) -> Result<PathBuf> {
    if is_in_library(source, library) {
        return Ok(source.to_path_buf());
    }
    std::fs::create_dir_all(library)
        .with_context(|| format!("Could not create library folder {}", library.display()))?;
    let file_name = source
        .file_name()
        .with_context(|| format!("{} has no file name", source.display()))?;

    let mut n = 1;
    let target = loop {
        let candidate = library.join(candidate_name(file_name, n));
        n += 1;
        if candidate.exists() {
            continue;
        }
        let mut tmp_name = OsString::from(".");
        tmp_name.push(candidate.file_name().unwrap_or(file_name));
        tmp_name.push(format!(".{}.tmp", std::process::id()));
        let tmp_path = library.join(tmp_name);
        let copied = (|| -> std::io::Result<()> {
            let mut from = std::fs::File::open(source)?;
            let mut file = std::fs::File::create(&tmp_path)?;
            std::io::copy(&mut from, &mut file)?;
            file.sync_all()
        })();
        if let Err(err) = copied {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err)
                .with_context(|| format!("Could not copy {} into the library", source.display()));
        }
        // Never overwrite: if someone took the name while we copied,
        // drop the temp file and try the next suffix.
        if candidate.exists() {
            let _ = std::fs::remove_file(&tmp_path);
            continue;
        }
        match std::fs::rename(&tmp_path, &candidate) {
            Ok(()) => break candidate,
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(err).with_context(|| {
                    format!("Could not copy {} into the library", source.display())
                });
            }
        }
    };

    carry_over_progress(library, source, &target)?;
    Ok(target)
}

/// Save the source deck's progress (sidecar, or migrated legacy
/// study-later list) as the copied deck's library state, if it
/// loaded fine and is non-empty.
fn carry_over_progress(library: &Path, source: &Path, target: &Path) -> Result<()> {
    let outcome = progress::load(source, &progress::state_path(source, None));
    if outcome.is_blocked() {
        // Never propagate a state file we could not read; the copy
        // itself succeeded and starts with empty progress.
        return Ok(());
    }
    let state = outcome.into_state();
    let empty = state.later.is_empty()
        && state.seen.is_empty()
        && state.reset_at == 0
        && state.opened_at == 0
        && state.prefs == progress::Prefs::default();
    if empty {
        return Ok(());
    }
    progress::save(&progress::state_path(target, Some(library)), &state).with_context(|| {
        format!(
            "Deck copied to {}, but its progress could not be carried over",
            target.display()
        )
    })?;
    Ok(())
}

/// Whether `text` is a Finder/iCloud copy number (`2`, `3`, …).
/// Numbered copies start at 2; `1` and non-numbers never match.
fn is_copy_number(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && text.parse::<u64>().is_ok_and(|n| n >= 2)
}

/// If `file_name` (a file in `.runki/`) is a sync conflict copy of
/// one of `deck_file_names`' state files, the deck file name it
/// copies. Recognized shapes, for a deck `French.apkg` whose state
/// file is `French.apkg.json`:
///
/// - Dropbox: `French.apkg (<anything> conflicted copy<anything>).json`
/// - numbered: `French.apkg 2.json`, `French.apkg 3.json`, …
/// - numbered after the state extension: `French.apkg.json 2`, …
///
/// A file that is exactly some deck's state-file name is never a
/// copy (a deck really called `French 2.apkg` owns
/// `French 2.apkg.json`). Deck names are tried longest first, so a
/// longer deck name that is a prefix of a copy's name wins over a
/// shorter one.
fn conflict_copy_deck<'a>(file_name: &str, deck_file_names: &'a [String]) -> Option<&'a str> {
    if deck_file_names
        .iter()
        .any(|deck| file_name == format!("{deck}.json"))
    {
        return None;
    }
    let mut decks: Vec<&str> = deck_file_names.iter().map(String::as_str).collect();
    decks.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    decks.into_iter().find(|deck| {
        // Dropbox: `<deck> (<anything> conflicted copy<anything>).json`
        if let Some(middle) = file_name
            .strip_prefix(&format!("{deck} ("))
            .and_then(|rest| rest.strip_suffix(").json"))
        {
            if middle.to_lowercase().contains("conflicted copy") {
                return true;
            }
        }
        // Numbered: `<deck> 2.json`, `<deck> 3.json`, …
        if let Some(number) = file_name
            .strip_prefix(&format!("{deck} "))
            .and_then(|rest| rest.strip_suffix(".json"))
        {
            if is_copy_number(number) {
                return true;
            }
        }
        // Numbered after the extension: `<deck>.json 2`, …
        if let Some(number) = file_name.strip_prefix(&format!("{deck}.json ")) {
            if is_copy_number(number) {
                return true;
            }
        }
        false
    })
}

/// Conflict copies in `runki_dir`, as `(copy path, deck file name)`
/// pairs sorted by path for determinism. A missing `.runki/`
/// folder simply has none.
fn find_conflict_copies(runki_dir: &Path, deck_file_names: &[String]) -> Vec<(PathBuf, String)> {
    let Ok(read_dir) = std::fs::read_dir(runki_dir) else {
        return Vec::new();
    };
    let mut copies = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(deck) = conflict_copy_deck(file_name, deck_file_names) {
            copies.push((path, deck.to_string()));
        }
    }
    copies.sort();
    copies
}

/// Merge every sync conflict copy in `<library>/.runki/` for the
/// decks in `library` into its deck's state, then delete the copy.
/// Runs in the background loader before summarizing, so it never
/// blocks the UI. The deck list comes from a fresh (stat-only)
/// [`scan`] of the library, so copies are matched against the
/// actual deck file names present — including not-downloaded
/// decks, whose real state files the exact-name guard must
/// protect even though they are never summarized.
///
/// A copy is left on disk untouched when it is dataless, does not
/// parse (including a newer state version), its deck's state is
/// dataless or blocked, or saving the merged state fails: the copy
/// may be the only record of another machine's progress, so it is
/// only deleted after a successful save.
fn merge_conflict_copies(library: &Path) {
    let Ok(entries) = scan(library) else {
        return;
    };
    let deck_paths: Vec<PathBuf> = entries.into_iter().map(|entry| entry.path).collect();
    let mut decks: HashMap<String, PathBuf> = HashMap::new();
    for deck_path in &deck_paths {
        if let Some(file_name) = deck_path.file_name().and_then(|name| name.to_str()) {
            decks
                .entry(file_name.to_string())
                .or_insert_with(|| deck_path.clone());
        }
    }
    let deck_file_names: Vec<String> = decks.keys().cloned().collect();
    let runki_dir = library.join(".runki");
    for (copy_path, deck_file_name) in find_conflict_copies(&runki_dir, &deck_file_names) {
        if is_dataless(&copy_path) {
            continue;
        }
        let Some(deck_path) = decks.get(&deck_file_name) else {
            continue;
        };
        let state_path = progress::state_path(deck_path, Some(library));
        if is_dataless(&state_path) {
            continue;
        }
        let Ok(Some(copy_state)) = progress::read_state_file(&copy_path) else {
            continue; // Unparseable, newer, or vanished: leave it alone.
        };
        let outcome = progress::load(deck_path, &state_path);
        if outcome.is_blocked() {
            continue;
        }
        let merged = outcome.state().merge(&copy_state);
        if progress::save(&state_path, &merged).is_ok() {
            // Only now is the copy's progress safely in the real
            // state file. A failed delete is fine: re-merging the
            // copy next scan is idempotent.
            let _ = std::fs::remove_file(&copy_path);
        }
    }
}

/// One background-loader result: a deck's counts and last-opened
/// time, tagged with the generation that requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckSummary {
    pub generation: u64,
    pub path: PathBuf,
    pub status: DeckStatus,
    pub opened_at: i64,
}

/// Parses a deck file into a [`Deck`]; injectable so tests can
/// count parses. The default is [`deck::load_deck`].
type Parser = Arc<dyn Fn(&Path) -> Result<Deck> + Send + Sync>;

/// Cache key for a parsed deck: same path, mtime, and size is
/// assumed to be the same deck, so returning to the library does
/// not re-parse it. Only card ids (or the parse error) are cached,
/// never full cards; the small state file is always re-read so
/// counts stay fresh.
type CacheKey = (PathBuf, Option<SystemTime>, u64);
type ParseCache = Arc<Mutex<HashMap<CacheKey, std::result::Result<Vec<String>, String>>>>;

/// Background loader for library rows.
///
/// [`start`](Self::start) spawns a worker thread that first merges
/// any sync conflict copies for these decks (`merge_conflict_copies`,
/// the loader's only write), then runs `deck::load_deck` +
/// `progress::load` for each deck and sends a [`DeckSummary`] back
/// over a channel, calling the repaint callback after each one.
/// Each `start` bumps a generation counter: workers from older
/// generations stop early and [`drain`](Self::drain) drops their
/// results. Placeholder decks are never parsed: a dataless or
/// placeholder-backed path summarizes as
/// [`DeckStatus::NotDownloaded`] without being read.
pub struct LibraryLoader {
    receiver: mpsc::Receiver<DeckSummary>,
    sender: mpsc::Sender<DeckSummary>,
    generation: Arc<AtomicU64>,
    repaint: Arc<Mutex<Box<dyn Fn() + Send>>>,
    cache: ParseCache,
    parser: Parser,
}

impl LibraryLoader {
    /// A loader whose `repaint` callback runs after each summary
    /// (the app wires it to `ctx.request_repaint()`).
    pub fn new(repaint: Box<dyn Fn() + Send>) -> Self {
        Self::with_parser(repaint, Arc::new(|path: &Path| deck::load_deck(path)))
    }

    fn with_parser(repaint: Box<dyn Fn() + Send>, parser: Parser) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            receiver,
            sender,
            generation: Arc::new(AtomicU64::new(0)),
            repaint: Arc::new(Mutex::new(repaint)),
            cache: Arc::new(Mutex::new(HashMap::new())),
            parser,
        }
    }

    /// The current generation; summaries from older ones are stale.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Start loading `paths` (decks in `library`) on a worker
    /// thread, superseding any load still running. Returns the new
    /// generation.
    pub fn start(&self, library: &Path, paths: Vec<PathBuf>) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let library = library.to_path_buf();
        let sender = self.sender.clone();
        let current = Arc::clone(&self.generation);
        let repaint = Arc::clone(&self.repaint);
        let cache = Arc::clone(&self.cache);
        let parser = Arc::clone(&self.parser);
        std::thread::spawn(move || {
            // Once per scan, before any deck's state is read for a
            // summary: fold sync conflict copies into the real
            // state files (and remove the copies).
            merge_conflict_copies(&library);
            for path in &paths {
                if current.load(Ordering::SeqCst) != generation {
                    return; // Superseded by a newer scan.
                }
                let summary = summarize(&library, path, generation, &cache, &parser);
                if current.load(Ordering::SeqCst) != generation {
                    return;
                }
                if sender.send(summary).is_err() {
                    return; // Loader dropped.
                }
                if let Ok(repaint) = repaint.lock() {
                    repaint();
                }
            }
        });
        generation
    }

    /// Take all summaries that have arrived, dropping any from a
    /// superseded generation.
    pub fn drain(&self) -> Vec<DeckSummary> {
        let current = self.generation();
        let mut summaries = Vec::new();
        while let Ok(summary) = self.receiver.try_recv() {
            if summary.generation == current {
                summaries.push(summary);
            }
        }
        summaries
    }
}

/// Load one deck (or reuse its cached parse) plus its fresh state,
/// and fold them into a summary. Never saves (conflict-copy
/// merging, the loader's only write, happens before summarizing).
///
/// A placeholder deck is reported [`DeckStatus::NotDownloaded`]
/// without being opened. A dataless state file is likewise skipped
/// — reading it would block on a download — and its progress is
/// treated as unknown (zero counts, `opened_at` 0); the deck itself
/// is still parsed for its total.
fn summarize(
    library: &Path,
    path: &Path,
    generation: u64,
    cache: &ParseCache,
    parser: &Parser,
) -> DeckSummary {
    if is_not_downloaded(path) {
        return DeckSummary {
            generation,
            path: path.to_path_buf(),
            status: DeckStatus::NotDownloaded,
            opened_at: 0,
        };
    }
    let state_path = progress::state_path(path, Some(library));
    let state = if is_dataless(&state_path) {
        progress::DeckState::new()
    } else {
        progress::load(path, &state_path).into_state()
    };
    let opened_at = state.opened_at;

    let metadata = std::fs::metadata(path);
    let key: CacheKey = (
        path.to_path_buf(),
        metadata.as_ref().ok().and_then(|m| m.modified().ok()),
        metadata.as_ref().map_or(0, std::fs::Metadata::len),
    );
    let cached = cache.lock().ok().and_then(|cache| cache.get(&key).cloned());
    let parsed = match cached {
        Some(parsed) => parsed,
        None => {
            let parsed = match parser(path) {
                Ok(deck) => Ok(deck.cards.iter().map(|card| card.id.clone()).collect()),
                Err(err) => Err(format!("{err:#}")),
            };
            if let Ok(mut cache) = cache.lock() {
                cache.insert(key, parsed.clone());
            }
            parsed
        }
    };

    let status = match parsed {
        Ok(ids) => DeckStatus::Ready {
            total: ids.len(),
            seen: state.seen_count(ids.iter().map(String::as_str)),
            later: state.later_count(ids.iter().map(String::as_str)),
        },
        Err(error) => DeckStatus::Failed(error),
    };
    DeckSummary {
        generation,
        path: path.to_path_buf(),
        status,
        opened_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::DeckState;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(test_name: &str) -> (PathBuf, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "runki-test-library-{test_name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let guard = TempDir(dir.clone());
        (dir, guard)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    fn names(entries: &[DeckEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect()
    }

    /// File (and dir) names directly in `dir`, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn seed_state(library: &Path, deck_name: &str, mutate: impl FnOnce(&mut DeckState)) {
        let deck_path = library.join(deck_name);
        let mut state = DeckState::new();
        mutate(&mut state);
        progress::save(&progress::state_path(&deck_path, Some(library)), &state)
            .expect("seed state");
    }

    #[test]
    fn scan_filters_and_lists_only_direct_deck_files() {
        let (dir, _temp) = temp_dir("scan-filters");
        write(&dir.join("French.csv"), "Q,A\n");
        write(
            &dir.join("German.APKG"),
            "not a real apkg, scan does not parse",
        );
        write(&dir.join("notes.TXT"), "Q\tA\n");
        write(&dir.join(".hidden.csv"), "Q,A\n");
        write(&dir.join(".French.csv.123.tmp"), "partial");
        write(&dir.join("partial.tmp"), "partial");
        write(&dir.join("French.csv.runki.json"), "{}");
        write(&dir.join("French.csv.later.json"), "[]");
        write(&dir.join("data.json"), "{}");
        write(&dir.join("noext"), "Q,A\n");
        write(&dir.join(".runki").join("French.csv.json"), "{}");
        write(&dir.join("sub").join("Inner.csv"), "Q,A\n");

        let entries = scan(&dir).expect("scan");
        // All opened_at are 0, so order is by name, case-insensitive.
        assert_eq!(names(&entries), ["French", "German", "notes"]);
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == DeckStatus::Loading),
            "scan rows start out loading"
        );
    }

    #[test]
    fn scan_missing_folder_is_a_clear_error() {
        let (dir, _temp) = temp_dir("scan-missing");
        let missing = dir.join("does-not-exist");
        let err = scan(&missing).expect_err("missing folder");
        let message = format!("{err:#}");
        assert!(
            message.contains("Could not read library folder") && message.contains("does-not-exist"),
            "{message}"
        );

        let file = dir.join("a-file");
        write(&file, "x");
        assert!(scan(&file).is_err(), "a file is not a readable folder");
    }

    #[test]
    fn scan_does_not_read_state_files() {
        let (dir, _temp) = temp_dir("scan-no-state-read");
        for name in ["banana.csv", "Apple.csv", "cherry.csv", "Dates.csv"] {
            write(&dir.join(name), "Q,A\n");
        }
        seed_state(&dir, "cherry.csv", |state| state.opened_at = 200);
        seed_state(&dir, "banana.csv", |state| state.opened_at = 100);

        // Scan only lists and stats entries (a state file could be a
        // cloud placeholder whose read would block), so every row
        // starts at opened_at 0 in name order; the loader's
        // summaries supply opened_at and trigger the re-sort.
        let entries = scan(&dir).expect("scan");
        assert_eq!(names(&entries), ["Apple", "banana", "cherry", "Dates"]);
        assert!(
            entries.iter().all(|entry| entry.opened_at == 0),
            "scan must not read state files: {entries:?}"
        );
    }

    #[test]
    fn sort_entries_orders_by_opened_at_then_name() {
        let entry = |name: &str, opened_at: i64| DeckEntry {
            path: PathBuf::from(format!("{name}.csv")),
            display_name: name.to_string(),
            status: DeckStatus::Loading,
            opened_at,
        };
        let mut entries = vec![
            entry("banana", 100),
            entry("Apple", 0),
            entry("cherry", 200),
            entry("Dates", 0),
        ];
        sort_entries(&mut entries);
        assert_eq!(names(&entries), ["cherry", "banana", "Apple", "Dates"]);
        assert_eq!(entries[0].opened_at, 200);
        assert_eq!(entries[2].opened_at, 0);
    }

    #[test]
    fn scan_tolerates_a_corrupt_state_file() {
        let (dir, _temp) = temp_dir("scan-corrupt-state");
        write(&dir.join("deck.csv"), "Q,A\n");
        write(
            &dir.join(".runki").join("deck.csv.json"),
            "not valid json {{{",
        );

        let entries = scan(&dir).expect("scan does not fail on bad state");
        assert_eq!(names(&entries), ["deck"]);
        assert_eq!(entries[0].opened_at, 0, "bad state reads as never opened");
    }

    fn write_state(path: &Path, state: &DeckState) {
        write(
            path,
            &serde_json::to_string(state).expect("serialize state"),
        );
    }

    #[test]
    fn dataless_check_is_false_for_local_and_missing_files() {
        // The SF_DATALESS flag itself cannot be created in a test;
        // only the negative cases and the name/path logic around it
        // are testable.
        let (dir, _temp) = temp_dir("dataless-negative");
        let local = dir.join("deck.csv");
        write(&local, "Q,A\n");
        assert!(!is_dataless(&local));
        assert!(!is_dataless(&dir.join("missing.csv")));
    }

    #[test]
    fn icloud_placeholder_name_and_path_logic() {
        assert_eq!(
            icloud_placeholder_real_name(".French.apkg.icloud").as_deref(),
            Some("French.apkg")
        );
        assert_eq!(
            icloud_placeholder_real_name(".my deck.csv.icloud").as_deref(),
            Some("my deck.csv")
        );
        assert_eq!(icloud_placeholder_real_name("French.apkg"), None);
        assert_eq!(icloud_placeholder_real_name("French.apkg.icloud"), None);
        assert_eq!(icloud_placeholder_real_name(".hidden.csv"), None);
        assert_eq!(icloud_placeholder_real_name(".icloud"), None);

        let real = Path::new("/lib/French.apkg");
        assert_eq!(
            icloud_placeholder_path(real),
            PathBuf::from("/lib/.French.apkg.icloud")
        );
    }

    #[test]
    fn reveal_path_prefers_real_file_then_placeholder() {
        let (dir, _temp) = temp_dir("reveal-path");
        let real = dir.join("French.apkg");
        let placeholder = dir.join(".French.apkg.icloud");
        assert_eq!(reveal_path(&real), real, "neither exists: real path");

        write(&placeholder, "placeholder");
        assert_eq!(reveal_path(&real), placeholder);

        write(&real, "deck");
        assert_eq!(reveal_path(&real), real);
    }

    #[test]
    fn scan_lists_icloud_placeholder_as_not_downloaded() {
        let (dir, _temp) = temp_dir("scan-icloud-placeholder");
        write(&dir.join(".French.apkg.icloud"), "placeholder");
        write(&dir.join(".German.csv.icloud"), "placeholder");
        // Placeholders for non-decks and hidden decks stay unlisted.
        write(&dir.join(".notes.md.icloud"), "placeholder");
        write(&dir.join("..hidden.csv.icloud"), "placeholder");
        write(&dir.join("Spanish.csv"), "Q,A\n");

        let entries = scan(&dir).expect("scan");
        assert_eq!(names(&entries), ["French", "German", "Spanish"]);
        let french = &entries[0];
        assert_eq!(french.path, dir.join("French.apkg"));
        assert_eq!(french.status, DeckStatus::NotDownloaded);
        assert_eq!(french.opened_at, 0);
        assert_eq!(entries[1].status, DeckStatus::NotDownloaded);
        assert_eq!(entries[2].status, DeckStatus::Loading);
    }

    #[test]
    fn scan_lists_real_file_only_when_placeholder_also_exists() {
        let (dir, _temp) = temp_dir("scan-icloud-real-wins");
        write(&dir.join("French.apkg"), "deck");
        write(&dir.join(".French.apkg.icloud"), "placeholder");

        let entries = scan(&dir).expect("scan");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, dir.join("French.apkg"));
        assert_eq!(entries[0].status, DeckStatus::Loading);
    }

    #[test]
    fn not_downloaded_rows_are_not_sent_to_the_loader() {
        let (dir, _temp) = temp_dir("not-downloaded-not-loaded");
        write(&dir.join(".French.apkg.icloud"), "placeholder");
        write(&dir.join("Spanish.csv"), "Q,A\n");

        let entries = scan(&dir).expect("scan");
        assert_eq!(loadable_paths(&entries), [dir.join("Spanish.csv")]);
    }

    #[test]
    fn conflict_copy_name_matching() {
        let decks: Vec<String> = ["French.apkg", "French 2.apkg", "German.csv"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let owner = |name: &str| conflict_copy_deck(name, &decks);

        // Dropbox pattern.
        assert_eq!(
            owner("French.apkg (Francisco's MacBook conflicted copy 2026-10-31).json"),
            Some("French.apkg")
        );
        assert_eq!(
            owner("French.apkg (conflicted copy).json"),
            Some("French.apkg")
        );
        // Numbered copies.
        assert_eq!(owner("French.apkg 2.json"), Some("French.apkg"));
        assert_eq!(owner("French.apkg 3.json"), Some("French.apkg"));
        assert_eq!(owner("French.apkg.json 2"), Some("French.apkg"));
        // Copies of the longer deck name belong to that deck.
        assert_eq!(owner("French 2.apkg 2.json"), Some("French 2.apkg"));
        assert_eq!(
            owner("French 2.apkg (Other Mac conflicted copy).json"),
            Some("French 2.apkg")
        );

        // Exact state-file names are never copies, including the
        // deck really called "French 2.apkg".
        assert_eq!(owner("French.apkg.json"), None);
        assert_eq!(owner("French 2.apkg.json"), None);
        assert_eq!(owner("German.csv.json"), None);
        // Unrelated or malformed names never match.
        assert_eq!(owner("Unrelated.apkg 2.json"), None);
        assert_eq!(owner("Unrelated.apkg (x conflicted copy).json"), None);
        assert_eq!(owner("French.apkg (just a copy).json"), None);
        assert_eq!(owner("French.apkg (conflicted).json"), None);
        assert_eq!(owner("French.apkg 1.json"), None);
        assert_eq!(owner("French.apkg x.json"), None);
        assert_eq!(owner("French.apkg 2.txt"), None);
        assert_eq!(owner("French.apkg.json"), None);
        assert_eq!(owner("random.json"), None);
        assert_eq!(owner(".French.apkg.json.123.tmp"), None);
    }

    #[test]
    fn conflict_copy_matching_uses_longest_deck_name() {
        // "Data.csv 2.csv" is itself a deck whose name extends
        // "Data.csv"-copy-looking prefixes; its copies must not be
        // claimed by the shorter deck, and its own state file must
        // never be treated as a copy at all.
        let decks: Vec<String> = ["Data.csv", "Data.csv 2.csv"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let owner = |name: &str| conflict_copy_deck(name, &decks);
        assert_eq!(owner("Data.csv 2.csv.json"), None, "exact state file");
        assert_eq!(
            owner("Data.csv 2.csv 2.json"),
            Some("Data.csv 2.csv"),
            "copy of the longer deck"
        );
        assert_eq!(
            owner("Data.csv 2.csv (x conflicted copy).json"),
            Some("Data.csv 2.csv")
        );
        assert_eq!(owner("Data.csv 2.json"), Some("Data.csv"));
        assert_eq!(owner("Data.csv (x conflicted copy).json"), Some("Data.csv"));
    }

    #[test]
    fn conflict_copy_is_merged_then_deleted() {
        let (dir, _temp) = temp_dir("conflict-merge");
        let deck_path = dir.join("French.csv");
        write(&deck_path, "Q1,A1\nQ2,A2\nQ3,A3\n");
        let deck = deck::load_deck(&deck_path).expect("parse deck");
        let (id1, id2, id3) = (
            deck.cards[0].id.as_str(),
            deck.cards[1].id.as_str(),
            deck.cards[2].id.as_str(),
        );

        // This Mac's state.
        seed_state(&dir, "French.csv", |state| {
            state.set_later(id1, true, 10);
            state.mark_seen(id1, 10);
            state.mark_opened(100);
        });
        // Another Mac's progress, left as a Dropbox conflict copy…
        let runki = dir.join(".runki");
        let dropbox_copy = runki.join("French.csv (Other Mac conflicted copy 2026-10-31).json");
        let mut other = DeckState::new();
        other.set_later(id2, true, 20);
        other.mark_seen(id2, 20);
        other.mark_opened(200);
        write_state(&dropbox_copy, &other);
        // …and a third Mac's, left as a numbered copy.
        let numbered_copy = runki.join("French.csv 2.json");
        let mut third = DeckState::new();
        third.set_later(id3, true, 30);
        third.mark_seen(id3, 30);
        write_state(&numbered_copy, &third);

        merge_conflict_copies(&dir);

        assert!(!dropbox_copy.exists(), "merged Dropbox copy removed");
        assert!(!numbered_copy.exists(), "merged numbered copy removed");
        let state_path = progress::state_path(&deck_path, Some(&dir));
        let merged = progress::load(&deck_path, &state_path).into_state();
        for id in [id1, id2, id3] {
            assert!(merged.is_later(id), "flag for {id} survived the merge");
            assert!(merged.is_seen(id), "seen mark for {id} survived");
        }
        assert_eq!(merged.opened_at, 200);
    }

    #[test]
    fn unparseable_and_newer_conflict_copies_are_left_in_place() {
        let (dir, _temp) = temp_dir("conflict-unparseable");
        let deck_path = dir.join("French.csv");
        write(&deck_path, "Q1,A1\n");
        let runki = dir.join(".runki");
        let garbage = runki.join("French.csv (x conflicted copy).json");
        write(&garbage, "not valid json {{{");
        let newer = runki.join("French.csv 2.json");
        write(&newer, "{\"version\": 999}");

        merge_conflict_copies(&dir);

        assert_eq!(
            std::fs::read_to_string(&garbage).expect("garbage copy kept"),
            "not valid json {{{"
        );
        assert!(newer.exists(), "newer-version copy kept");
        assert!(
            !progress::state_path(&deck_path, Some(&dir)).exists(),
            "no state file invented from an unreadable copy"
        );
    }

    #[test]
    fn conflict_copy_is_kept_when_the_target_state_is_blocked() {
        let (dir, _temp) = temp_dir("conflict-blocked-target");
        let deck_path = dir.join("French.csv");
        write(&deck_path, "Q1,A1\n");
        // Block the real state path with a directory: the state
        // cannot be read or written there.
        let state_path = progress::state_path(&deck_path, Some(&dir));
        std::fs::create_dir_all(&state_path).expect("blocking dir");
        let copy = dir.join(".runki").join("French.csv 2.json");
        let mut other = DeckState::new();
        other.mark_opened(200);
        write_state(&copy, &other);

        merge_conflict_copies(&dir);

        assert!(copy.exists(), "copy kept when its progress cannot be saved");
        assert!(state_path.is_dir(), "blocked target untouched");
    }

    #[test]
    fn merge_conflict_copies_never_touches_a_real_decks_state() {
        let (dir, _temp) = temp_dir("conflict-real-deck-2");
        let french = dir.join("French.csv");
        write(&french, "Q1,A1\n");
        let french2 = dir.join("French 2.csv");
        write(&french2, "Q2,A2\n");
        // The real state of the deck called "French 2.csv".
        seed_state(&dir, "French 2.csv", |state| state.mark_opened(555));
        let french2_state = progress::state_path(&french2, Some(&dir));
        let before = std::fs::read_to_string(&french2_state).expect("state seeded");

        merge_conflict_copies(&dir);

        assert_eq!(
            std::fs::read_to_string(&french2_state).expect("state still there"),
            before,
            "a real deck's state file is not treated as a conflict copy"
        );
        assert!(
            !progress::state_path(&french, Some(&dir)).exists(),
            "nothing was merged into the other deck either"
        );
    }

    #[test]
    fn last_opened_labels() {
        const DAY: i64 = 86_400_000;
        let now = 100 * DAY + 5_000;
        assert_eq!(last_opened_label(0, now), "never");
        assert_eq!(last_opened_label(-1, now), "never");
        assert_eq!(last_opened_label(now, now), "today");
        assert_eq!(last_opened_label(now - 1_000, now), "today");
        assert_eq!(last_opened_label(99 * DAY, now), "yesterday");
        assert_eq!(last_opened_label(97 * DAY, now), "3 days ago");
    }

    #[test]
    fn add_deck_copies_with_numbered_suffixes() {
        let (dir, _temp) = temp_dir("add-suffix");
        let library = dir.join("library");
        let source_dir = dir.join("downloads");
        write(&library.join("French.apkg"), "original");
        let source = source_dir.join("French.apkg");
        write(&source, "from downloads");

        let second = add_deck(&library, &source).expect("add second");
        assert_eq!(second, library.join("French (2).apkg"));
        let third = add_deck(&library, &source).expect("add third");
        assert_eq!(third, library.join("French (3).apkg"));

        assert_eq!(
            std::fs::read_to_string(library.join("French.apkg")).expect("read"),
            "original",
            "existing deck is never overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(&second).expect("read"),
            "from downloads"
        );
        assert_eq!(
            std::fs::read_to_string(&third).expect("read"),
            "from downloads"
        );
        assert_eq!(
            listing(&library),
            ["French (2).apkg", "French (3).apkg", "French.apkg"],
            "no temp or partial files left behind"
        );
    }

    #[test]
    fn add_deck_keeps_name_without_clash_and_carries_sidecar_progress() {
        let (dir, _temp) = temp_dir("add-carry-sidecar");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("library");
        let source = dir.join("deck.csv");
        write(&source, "Q1,A1\nQ2,A2\n");
        let deck = deck::load_deck(&source).expect("parse source");
        let mut state = DeckState::new();
        state.set_later(&deck.cards[0].id, true, 111);
        state.mark_seen(&deck.cards[1].id, 222);
        state.mark_opened(333);
        progress::save(&progress::state_path(&source, None), &state).expect("save sidecar");

        let added = add_deck(&library, &source).expect("add");
        assert_eq!(added, library.join("deck.csv"));

        let carried = progress::load(&added, &progress::state_path(&added, Some(&library)));
        assert_eq!(carried.state(), &state, "sidecar progress carried over");
        assert_eq!(
            listing(&library),
            [".runki", "deck.csv"],
            "only the deck and its state folder, no temp files"
        );
    }

    #[test]
    fn add_deck_carries_legacy_later_progress() {
        let (dir, _temp) = temp_dir("add-carry-legacy");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("library");
        let source = dir.join("deck.csv");
        write(&source, "Q1,A1\n");
        write(&dir.join("deck.csv.later.json"), "[\"id-1\",\"id-2\"]");

        let added = add_deck(&library, &source).expect("add");
        let carried = progress::load(&added, &progress::state_path(&added, Some(&library)));
        assert!(carried.state().is_later("id-1"));
        assert!(carried.state().is_later("id-2"));
    }

    #[test]
    fn add_deck_without_progress_writes_no_state() {
        let (dir, _temp) = temp_dir("add-no-progress");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("library");
        let source = dir.join("deck.csv");
        write(&source, "Q1,A1\n");

        let added = add_deck(&library, &source).expect("add");
        assert!(
            !progress::state_path(&added, Some(&library)).exists(),
            "nothing to carry over, no state file invented"
        );
    }

    #[test]
    fn add_deck_source_in_library_is_a_no_op() {
        let (dir, _temp) = temp_dir("add-no-op");
        let library = dir.join("library");
        let source = library.join("deck.csv");
        write(&source, "Q1,A1\n");

        let added = add_deck(&library, &source).expect("add");
        assert_eq!(added, source);
        assert_eq!(listing(&library), ["deck.csv"], "no copy was made");
    }

    #[test]
    fn add_deck_missing_source_errors_and_leaves_no_temp_files() {
        let (dir, _temp) = temp_dir("add-missing");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("library");

        assert!(add_deck(&library, &dir.join("ghost.csv")).is_err());
        assert!(listing(&library).is_empty());
    }

    /// Drain until `expected` summaries arrive or the deadline hits.
    fn collect(loader: &LibraryLoader, expected: usize) -> Vec<DeckSummary> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut summaries = Vec::new();
        while summaries.len() < expected {
            summaries.extend(loader.drain());
            assert!(
                Instant::now() < deadline,
                "timed out waiting for summaries: {summaries:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        summaries
    }

    fn no_op_loader() -> LibraryLoader {
        LibraryLoader::new(Box::new(|| {}))
    }

    #[test]
    fn loader_reports_counts_and_failures() {
        let (dir, _temp) = temp_dir("loader-e2e");
        let good = dir.join("good.csv");
        write(&good, "Q1,A1\nQ2,A2\n");
        let broken = dir.join("broken.txt");
        write(&broken, "just some notes\nwith no separator\n");

        let deck = deck::load_deck(&good).expect("parse good");
        seed_state(&dir, "good.csv", |state| {
            state.mark_seen(&deck.cards[0].id, 10);
            state.set_later(&deck.cards[1].id, true, 10);
            state.mark_opened(4242);
        });

        let loader = no_op_loader();
        let generation = loader.start(&dir, vec![good.clone(), broken.clone()]);
        let summaries = collect(&loader, 2);

        let good_summary = summaries
            .iter()
            .find(|s| s.path == good)
            .expect("good summary");
        assert_eq!(good_summary.generation, generation);
        assert_eq!(
            good_summary.status,
            DeckStatus::Ready {
                total: 2,
                seen: 1,
                later: 1
            }
        );
        assert_eq!(good_summary.opened_at, 4242);

        let broken_summary = summaries
            .iter()
            .find(|s| s.path == broken)
            .expect("broken summary");
        assert!(
            matches!(broken_summary.status, DeckStatus::Failed(_)),
            "broken deck stays listed with its error: {:?}",
            broken_summary.status
        );
    }

    #[test]
    fn opened_at_arrives_via_the_loader_not_scan() {
        let (dir, _temp) = temp_dir("opened-via-loader");
        let deck_path = dir.join("deck.csv");
        write(&deck_path, "Q1,A1\n");
        seed_state(&dir, "deck.csv", |state| state.mark_opened(4242));

        let entries = scan(&dir).expect("scan");
        assert_eq!(entries[0].opened_at, 0, "scan reads no state file");

        let loader = no_op_loader();
        loader.start(&dir, loadable_paths(&entries));
        let summaries = collect(&loader, 1);
        assert_eq!(summaries[0].opened_at, 4242);
    }

    #[test]
    fn loader_never_parses_a_not_downloaded_deck() {
        let (dir, _temp) = temp_dir("loader-not-downloaded");
        write(&dir.join(".French.csv.icloud"), "placeholder");
        let real_path = dir.join("French.csv");

        let parses = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&parses);
        let loader = LibraryLoader::with_parser(
            Box::new(|| {}),
            Arc::new(move |path: &Path| {
                counting.fetch_add(1, Ordering::SeqCst);
                deck::load_deck(path)
            }),
        );
        loader.start(&dir, vec![real_path.clone()]);
        let summaries = collect(&loader, 1);
        assert_eq!(summaries[0].path, real_path);
        assert_eq!(summaries[0].status, DeckStatus::NotDownloaded);
        assert_eq!(summaries[0].opened_at, 0);
        assert_eq!(
            parses.load(Ordering::SeqCst),
            0,
            "a placeholder deck is never opened"
        );
    }

    #[test]
    fn loader_merges_conflict_copies_before_summarizing() {
        let (dir, _temp) = temp_dir("loader-conflict-merge");
        let deck_path = dir.join("French.csv");
        write(&deck_path, "Q1,A1\nQ2,A2\n");
        let deck = deck::load_deck(&deck_path).expect("parse deck");
        seed_state(&dir, "French.csv", |state| {
            state.mark_seen(&deck.cards[0].id, 10);
        });
        let copy = dir
            .join(".runki")
            .join("French.csv (Other Mac conflicted copy).json");
        let mut other = DeckState::new();
        other.mark_seen(&deck.cards[1].id, 20);
        other.set_later(&deck.cards[1].id, true, 20);
        other.mark_opened(777);
        write_state(&copy, &other);

        let loader = no_op_loader();
        loader.start(&dir, vec![deck_path.clone()]);
        let summaries = collect(&loader, 1);

        assert!(!copy.exists(), "loader merged and removed the copy");
        assert_eq!(
            summaries[0].status,
            DeckStatus::Ready {
                total: 2,
                seen: 2,
                later: 1
            },
            "summary counts include the merged copy's progress"
        );
        assert_eq!(summaries[0].opened_at, 777);
    }

    #[test]
    fn loader_drops_stale_generation_results() {
        let (dir, _temp) = temp_dir("loader-stale");
        let first = dir.join("first.csv");
        write(&first, "Q1,A1\n");
        let second = dir.join("second.csv");
        write(&second, "Q2,A2\n");

        let loader = no_op_loader();
        let old = loader.start(&dir, vec![first]);
        let new = loader.start(&dir, vec![second.clone()]);
        assert!(new > old);

        // Whatever the first worker managed to send is dropped by
        // drain(); only the newer scan's results come through.
        let summaries = collect(&loader, 1);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].generation, new);
        assert_eq!(summaries[0].path, second);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            loader.drain().is_empty(),
            "no stale summary arrives late either"
        );
    }

    #[test]
    fn loader_cache_hit_skips_reparse_but_rereads_state() {
        let (dir, _temp) = temp_dir("loader-cache");
        let deck_path = dir.join("deck.csv");
        write(&deck_path, "Q1,A1\nQ2,A2\n");
        let deck = deck::load_deck(&deck_path).expect("parse");

        let parses = Arc::new(AtomicUsize::new(0));
        let counting = Arc::clone(&parses);
        let loader = LibraryLoader::with_parser(
            Box::new(|| {}),
            Arc::new(move |path: &Path| {
                counting.fetch_add(1, Ordering::SeqCst);
                deck::load_deck(path)
            }),
        );

        loader.start(&dir, vec![deck_path.clone()]);
        let first = collect(&loader, 1);
        assert_eq!(
            first[0].status,
            DeckStatus::Ready {
                total: 2,
                seen: 0,
                later: 0
            }
        );
        assert_eq!(parses.load(Ordering::SeqCst), 1);

        // Progress changes on disk; the deck file does not.
        seed_state(&dir, "deck.csv", |state| {
            state.mark_seen(&deck.cards[0].id, 10);
        });
        loader.start(&dir, vec![deck_path.clone()]);
        let second = collect(&loader, 1);
        assert_eq!(
            second[0].status,
            DeckStatus::Ready {
                total: 2,
                seen: 1,
                later: 0
            },
            "state file is re-read even on a cache hit"
        );
        assert_eq!(
            parses.load(Ordering::SeqCst),
            1,
            "unchanged deck is not re-parsed"
        );

        // Changing the deck (different size) busts the cache.
        write(&deck_path, "Q1,A1\nQ2,A2\nQ3,A3\n");
        loader.start(&dir, vec![deck_path]);
        let third = collect(&loader, 1);
        assert_eq!(
            third[0].status,
            DeckStatus::Ready {
                total: 3,
                seen: 1,
                later: 0
            }
        );
        assert_eq!(parses.load(Ordering::SeqCst), 2);
    }
}
