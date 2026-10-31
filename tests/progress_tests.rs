use std::path::{Path, PathBuf};

use runki::progress::{
    load, save, state_path, CardFilter, CardOrder, DeckState, LaterEntry, LoadOutcome, Prefs,
};

fn legacy_path(deck_path: &Path) -> PathBuf {
    let mut os = deck_path.as_os_str().to_owned();
    os.push(".later.json");
    PathBuf::from(os)
}

fn deck_in(dir: &Path) -> PathBuf {
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();
    deck_path
}

#[test]
fn missing_file_gives_ok_empty() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);

    let outcome = load(&deck_path, &path);
    assert!(matches!(outcome, LoadOutcome::Ok(_)));
    assert!(!outcome.is_blocked());
    assert_eq!(outcome.state(), &DeckState::new());
}

#[test]
fn save_then_load_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    let mut state = DeckState::new();
    state.set_later("abc", true, 10);
    state.set_later("123", false, 20);
    state.mark_seen("abc", 30);
    state.mark_opened(40);

    let saved = save(&path, &state).unwrap();
    assert_eq!(saved, state);

    let outcome = load(&deck_path, &path);
    assert_eq!(outcome.state(), &state);
    assert!(dir.path().join("deck.tsv.runki.json").exists());
}

#[test]
fn save_leaves_no_tmp_files() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    let mut state = DeckState::new();
    state.set_later("abc", true, 1);

    save(&path, &state).unwrap();

    assert!(path.exists());
    let tmp_left = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!tmp_left, "atomic save must not leave temp files");
}

#[test]
fn save_creates_missing_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".runki").join("deck.tsv.json");
    let mut state = DeckState::new();
    state.set_later("abc", true, 1);

    save(&path, &state).unwrap();
    assert!(path.exists());
}

#[test]
fn invalid_json_blocks_saving_and_leaves_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    let corrupt = "not valid json {{{";
    std::fs::write(&path, corrupt).unwrap();

    let outcome = load(&deck_path, &path);
    assert!(outcome.is_blocked(), "corrupt JSON blocks saving");
    assert_eq!(outcome.state(), &DeckState::new());
    assert!(
        format!("{:#}", outcome.error().expect("error")).contains("deck.tsv.runki.json"),
        "error should name the file"
    );

    // Save re-reads the file and must also refuse to overwrite it.
    let mut state = DeckState::new();
    state.set_later("abc", true, 1);
    assert!(save(&path, &state).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
}

#[test]
fn newer_version_blocks_saving_and_leaves_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    let future = r#"{"version": 2, "later": {}, "seen": {}}"#;
    std::fs::write(&path, future).unwrap();

    let outcome = load(&deck_path, &path);
    assert!(outcome.is_blocked(), "version > 1 blocks saving");

    let state = DeckState::new();
    assert!(save(&path, &state).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), future);
}

#[test]
fn missing_fields_deserialize_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    std::fs::write(&path, "{}").unwrap();

    let outcome = load(&deck_path, &path);
    assert!(!outcome.is_blocked());
    assert_eq!(outcome.state(), &DeckState::new());
}

#[test]
fn save_merges_with_what_is_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);

    // Another machine saved a flag and a seen mark.
    let mut disk = DeckState::new();
    disk.set_later("disk-card", true, 100);
    disk.mark_seen("shared", 100);
    save(&path, &disk).unwrap();

    // This machine saves from stale memory with its own changes.
    let mut mem = DeckState::new();
    mem.set_later("mem-card", true, 200);
    mem.mark_seen("shared", 50);
    let merged = save(&path, &mem).unwrap();

    assert!(merged.is_later("disk-card"), "disk flag survives");
    assert!(merged.is_later("mem-card"), "memory flag is written");
    assert_eq!(merged.seen["shared"], 100, "seen keeps the max");
    // And the merged result really is what is on disk now.
    assert_eq!(load(&deck_path, &path).state(), &merged);
}

#[test]
fn merge_two_machines_unflag_stays_unflagged() {
    // Both machines start from a state where X is flagged.
    let mut base = DeckState::new();
    base.set_later("x", true, 10);

    // Machine A unflags X (newer write).
    let mut a = base.clone();
    a.set_later("x", false, 20);
    // Machine B, still on the stale copy, flags Y.
    let mut b = base;
    b.set_later("y", true, 15);

    let merged = a.merge(&b);
    assert!(!merged.is_later("x"), "A's newer unflag wins");
    assert_eq!(
        merged.later["x"],
        LaterEntry { on: false, t: 20 },
        "unflag is kept as on:false, not deleted"
    );
    assert!(merged.is_later("y"), "B's flag is kept too");
    assert_eq!(merged, b.merge(&a), "merge is commutative here");
}

#[test]
fn merge_rules_and_ties() {
    let mut a = DeckState::new();
    a.set_later("tie", false, 10);
    a.set_later("a-newer", true, 30);
    a.mark_seen("s", 5);
    a.reset_at = 3;
    a.opened_at = 7;
    a.prefs = Prefs {
        t: 40,
        order: CardOrder::Deck,
        filter: CardFilter::All,
        shuffle_seed: 1,
        position: Some("a".to_string()),
    };

    let mut b = DeckState::new();
    b.set_later("tie", true, 10);
    b.set_later("a-newer", false, 20);
    b.mark_seen("s", 9);
    b.reset_at = 4;
    b.opened_at = 6;
    b.prefs = Prefs {
        t: 40,
        order: CardOrder::Shuffled,
        filter: CardFilter::Unseen,
        shuffle_seed: 2,
        position: Some("b".to_string()),
    };

    let merged = a.merge(&b);
    assert!(merged.is_later("tie"), "later tie goes to on:true");
    assert!(merged.is_later("a-newer"), "larger t wins");
    assert_eq!(merged.seen["s"], 9, "seen is max");
    assert_eq!(merged.reset_at, 4, "reset_at is max");
    assert_eq!(merged.opened_at, 7, "opened_at is max");
    assert_eq!(merged.prefs, a.prefs, "prefs tie keeps self (a)");

    // With strictly newer prefs on b, b's prefs win either way round.
    let mut b2 = b.clone();
    b2.prefs.t = 41;
    assert_eq!(a.merge(&b2).prefs, b2.prefs);
    assert_eq!(b2.merge(&a).prefs, b2.prefs);
}

#[test]
fn merge_is_commutative_associative_idempotent() {
    let mut a = DeckState::new();
    a.set_later("x", true, 10);
    a.set_later("y", false, 30);
    a.mark_seen("x", 11);
    a.reset_at = 2;
    a.opened_at = 5;
    a.prefs.t = 10;

    let mut b = DeckState::new();
    b.set_later("x", false, 20);
    b.set_later("z", true, 1);
    b.mark_seen("x", 9);
    b.mark_seen("z", 3);
    b.reset_at = 4;
    b.opened_at = 3;
    b.prefs.t = 20;

    let mut c = DeckState::new();
    c.set_later("y", true, 25);
    c.mark_seen("y", 26);
    c.opened_at = 8;
    c.prefs.t = 15;

    // Prefs timestamps are distinct, so the prefs tie rule cannot fire
    // and merge must be fully commutative/associative/idempotent.
    assert_eq!(a.merge(&b), b.merge(&a), "commutative");
    assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)), "associative");
    assert_eq!(a.merge(&a), a, "idempotent");
    assert_eq!(b.merge(&b), b, "idempotent");
    let all = a.merge(&b).merge(&c);
    assert_eq!(all.merge(&all), all, "merged result is idempotent");
}

#[test]
fn reset_semantics_seen_at_reset_at_is_not_seen() {
    let mut state = DeckState::new();
    state.mark_seen("before", 99);
    state.mark_seen("at", 100);
    state.mark_seen("after", 101);
    state.set_later("before", true, 50);

    state.reset(100);

    assert!(!state.is_seen("before"));
    assert!(!state.is_seen("at"), "seen exactly at reset_at is not seen");
    assert!(state.is_seen("after"));
    assert_eq!(state.seen.len(), 3, "reset leaves seen times alone");
    assert!(state.is_later("before"), "reset keeps study-later flags");
}

#[test]
fn counts_ignore_stale_ids() {
    let mut state = DeckState::new();
    state.set_later("a", true, 1);
    state.set_later("stale", true, 1);
    state.set_later("b", false, 1);
    state.mark_seen("a", 10);
    state.mark_seen("stale", 10);
    state.mark_seen("b", 10);

    let ids = ["a", "b", "c"];
    assert_eq!(state.later_count(ids.iter().copied()), 1);
    assert_eq!(state.seen_count(ids.iter().copied()), 2);
    // Stale ids are kept in the state, just not counted.
    assert!(state.is_later("stale"));
    assert!(state.is_seen("stale"));
}

#[test]
fn migration_imports_legacy_file_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    let legacy = legacy_path(&deck_path);
    std::fs::write(&legacy, r#"["abc", "123"]"#).unwrap();

    let outcome = load(&deck_path, &path);
    assert!(outcome.is_migrated());
    assert!(!outcome.is_blocked());
    assert!(outcome.state().is_later("abc"));
    assert!(outcome.state().is_later("123"));
    assert_eq!(
        outcome.state().later["abc"],
        LaterEntry { on: true, t: 0 },
        "legacy ids import as on:true at t:0"
    );
    assert!(legacy.exists(), "legacy file is kept");

    // Once saved, the new file is authoritative and legacy is ignored.
    let mut state = outcome.into_state();
    state.set_later("abc", false, 500);
    save(&path, &state).unwrap();
    let outcome = load(&deck_path, &path);
    assert!(!outcome.is_migrated());
    assert!(!outcome.state().is_later("abc"));
    assert!(outcome.state().is_later("123"));
    assert!(legacy.exists(), "legacy file is still kept");
}

#[test]
fn unreadable_legacy_file_blocks_saving() {
    let dir = tempfile::tempdir().unwrap();
    let deck_path = deck_in(dir.path());
    let path = state_path(&deck_path, None);
    std::fs::write(legacy_path(&deck_path), "not valid json {{{").unwrap();

    let outcome = load(&deck_path, &path);
    assert!(outcome.is_blocked());
    assert_eq!(outcome.state(), &DeckState::new());
}

#[test]
fn state_path_sidecar_without_library() {
    assert_eq!(
        state_path(Path::new("/some/dir/deck.csv"), None),
        PathBuf::from("/some/dir/deck.csv.runki.json")
    );
    assert_eq!(
        state_path(Path::new("deck.apkg"), None),
        PathBuf::from("deck.apkg.runki.json")
    );
    assert_eq!(
        state_path(Path::new("no-extension"), None),
        PathBuf::from("no-extension.runki.json")
    );
}

#[test]
fn state_path_uses_library_dir_for_library_decks() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    let deck_path = library.join("deck.apkg");
    std::fs::write(&deck_path, "fake").unwrap();

    assert_eq!(
        state_path(&deck_path, Some(&library)),
        library.join(".runki").join("deck.apkg.json")
    );

    // A deck elsewhere still gets a sidecar, library or not.
    let elsewhere = dir.path().join("elsewhere.apkg");
    std::fs::write(&elsewhere, "fake").unwrap();
    assert_eq!(
        state_path(&elsewhere, Some(&library)),
        dir.path().join("elsewhere.apkg.runki.json")
    );

    // A missing library directory cannot match: sidecar fallback.
    assert_eq!(
        state_path(&deck_path, Some(Path::new("/nonexistent/library"))),
        library.join("deck.apkg.runki.json")
    );
}

#[test]
fn enums_serialize_lowercase() {
    let mut state = DeckState::new();
    state.prefs = Prefs {
        t: 1,
        order: CardOrder::Shuffled,
        filter: CardFilter::Later,
        shuffle_seed: 42,
        position: None,
    };
    let text = serde_json::to_string(&state).unwrap();
    assert!(text.contains(r#""order":"shuffled""#), "{text}");
    assert!(text.contains(r#""filter":"later""#), "{text}");
    assert!(text.contains(r#""version":1"#), "{text}");

    let parsed: DeckState =
        serde_json::from_str(r#"{"prefs":{"order":"deck","filter":"unseen"}}"#).unwrap();
    assert_eq!(parsed.prefs.order, CardOrder::Deck);
    assert_eq!(parsed.prefs.filter, CardFilter::Unseen);
    assert_eq!(parsed.version, 1, "missing version defaults to 1");
}
