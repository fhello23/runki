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
//! Phase 4 will add conflict-copy merging and not-downloaded
//! (dataless) detection; [`DeckStatus::NotDownloaded`] already
//! leaves room for the latter, but nothing produces it yet.

use std::collections::HashMap;
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
    /// The file is an online-only placeholder (phase 4 detection).
    /// Never read by the background loader.
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

/// List the decks directly in `dir`, sorted by last opened (newest
/// first), then name (case-insensitive).
///
/// Rows come back as [`DeckStatus::Loading`]; `opened_at` is read
/// from each deck's (small) state file so the order is right at
/// once. A state file that fails to load just gives `opened_at`
/// 0 — it never fails the scan. A missing or unreadable folder is
/// an `Err` with a message naming the folder.
pub fn scan(dir: &Path) -> Result<Vec<DeckEntry>> {
    let read_dir = std::fs::read_dir(dir)
        .with_context(|| format!("Could not read library folder {}", dir.display()))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry =
            entry.with_context(|| format!("Could not read library folder {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_hidden_or_sidecar(file_name) || !is_deck_path(&path) {
            continue;
        }
        let display_name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(file_name)
            .to_string();
        let state_path = progress::state_path(&path, Some(dir));
        let opened_at = progress::load(&path, &state_path).state().opened_at;
        entries.push(DeckEntry {
            path,
            display_name,
            status: DeckStatus::Loading,
            opened_at,
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

/// Library-folder shortcuts for first launch: `…/Dropbox/Runki` and
/// `…/CloudDocs/Runki` under the home folder, offered only when
/// their parent exists (choosing one creates the `Runki` folder).
pub fn suggested_library_folders() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| suggested_library_folders_in(&home))
        .unwrap_or_default()
}

fn suggested_library_folders_in(home: &Path) -> Vec<PathBuf> {
    [
        home.join("Library")
            .join("CloudStorage")
            .join("Dropbox")
            .join("Runki"),
        home.join("Dropbox").join("Runki"),
        home.join("Library")
            .join("Mobile Documents")
            .join("com~apple~CloudDocs")
            .join("Runki"),
    ]
    .into_iter()
    .filter(|path| path.parent().is_some_and(Path::is_dir))
    .collect()
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
/// [`start`](Self::start) spawns a worker thread that runs
/// `deck::load_deck` + `progress::load` (read-only, never saves)
/// for each deck and sends a [`DeckSummary`] back over a channel,
/// calling the repaint callback after each one. Each `start` bumps
/// a generation counter: workers from older generations stop early
/// and [`drain`](Self::drain) drops their results.
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
/// and fold them into a summary. Read-only: never saves.
fn summarize(
    library: &Path,
    path: &Path,
    generation: u64,
    cache: &ParseCache,
    parser: &Parser,
) -> DeckSummary {
    let state_path = progress::state_path(path, Some(library));
    let outcome = progress::load(path, &state_path);
    let state = outcome.state();
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
    fn scan_sorts_by_opened_at_then_name() {
        let (dir, _temp) = temp_dir("scan-sorting");
        for name in ["banana.csv", "Apple.csv", "cherry.csv", "Dates.csv"] {
            write(&dir.join(name), "Q,A\n");
        }
        seed_state(&dir, "cherry.csv", |state| state.opened_at = 200);
        seed_state(&dir, "banana.csv", |state| state.opened_at = 100);

        let entries = scan(&dir).expect("scan");
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
    fn suggested_folders_need_an_existing_parent() {
        let (home, _temp) = temp_dir("suggested");
        assert!(suggested_library_folders_in(&home).is_empty());

        std::fs::create_dir_all(home.join("Dropbox")).expect("dropbox parent");
        std::fs::create_dir_all(
            home.join("Library")
                .join("Mobile Documents")
                .join("com~apple~CloudDocs"),
        )
        .expect("icloud parent");
        let suggested = suggested_library_folders_in(&home);
        assert_eq!(
            suggested,
            [
                home.join("Dropbox").join("Runki"),
                home.join("Library")
                    .join("Mobile Documents")
                    .join("com~apple~CloudDocs")
                    .join("Runki"),
            ]
        );
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
