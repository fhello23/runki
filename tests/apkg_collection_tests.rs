use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use runki::deck::load_deck;

#[test]
fn prefers_anki21_over_dummy_anki2() {
    let dir = temp_dir("apkg-anki21");
    let dummy = db_bytes(
        &dir,
        "dummy.anki2",
        &["This file requires a newer version of Anki."],
    );
    let real = db_bytes(
        &dir,
        "real.anki21",
        &["Capital of France?\u{1f}Paris", "Water formula\u{1f}H2O"],
    );
    let apkg_path = dir.join("test.apkg");
    write_apkg(
        &apkg_path,
        &[("collection.anki2", dummy), ("collection.anki21", real)],
    );

    let deck = load_deck(&apkg_path).unwrap();
    assert_eq!(deck.cards.len(), 2);
    assert_eq!(deck.cards[0].front, "Capital of France?");
    assert_eq!(deck.cards[0].back, "Paris");
    assert_eq!(deck.cards[1].front, "Water formula");
    assert_eq!(deck.cards[1].back, "H2O");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loads_zstd_compressed_anki21b_over_dummy_anki2() {
    let dir = temp_dir("apkg-anki21b");
    let dummy = db_bytes(
        &dir,
        "dummy.anki2",
        &["This file requires a newer version of Anki."],
    );
    let real = db_bytes(
        &dir,
        "real.anki21b",
        &["Capital of France?\u{1f}Paris", "Water formula\u{1f}H2O"],
    );
    let compressed = zstd::encode_all(&real[..], 0).unwrap();
    let apkg_path = dir.join("test.apkg");
    write_apkg(
        &apkg_path,
        &[
            ("collection.anki2", dummy),
            ("collection.anki21b", compressed),
        ],
    );

    let deck = load_deck(&apkg_path).unwrap();
    assert_eq!(deck.cards.len(), 2);
    assert_eq!(deck.cards[0].front, "Capital of France?");
    assert_eq!(deck.cards[0].back, "Paris");
    assert_eq!(deck.cards[1].front, "Water formula");
    assert_eq!(deck.cards[1].back, "H2O");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_anki21b_is_a_clear_error() {
    let dir = temp_dir("apkg-anki21b-corrupt");
    let apkg_path = dir.join("test.apkg");
    write_apkg(
        &apkg_path,
        &[("collection.anki21b", b"not zstd data".to_vec())],
    );

    let err = load_deck(&apkg_path).unwrap_err().to_string();
    assert!(
        err.contains("collection.anki21b"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Build a minimal Anki SQLite database; one card per note in `notes`.
fn db_bytes(dir: &Path, name: &str, notes: &[&str]) -> Vec<u8> {
    let db_path = dir.join(name);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, flds TEXT);
             CREATE TABLE cards (id INTEGER PRIMARY KEY, nid INTEGER);",
        )
        .unwrap();
        for (i, flds) in notes.iter().enumerate() {
            let id = i as i64 + 1;
            conn.execute(
                "INSERT INTO notes VALUES (?1, ?2)",
                rusqlite::params![id, flds],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cards VALUES (?1, ?2)",
                rusqlite::params![id * 10, id],
            )
            .unwrap();
        }
    }
    std::fs::read(&db_path).unwrap()
}

fn write_apkg(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    let apkg_bytes = zip.finish().unwrap().into_inner();
    std::fs::write(path, apkg_bytes).unwrap();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("runki-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
