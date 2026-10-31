use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use runki::deck::{later_path, load_deck};

#[test]
fn later_path_keeps_name_for_normal_paths() {
    assert_eq!(
        later_path(Path::new("/some/dir/deck.csv")),
        PathBuf::from("/some/dir/deck.csv.later.json")
    );
    assert_eq!(
        later_path(Path::new("deck.apkg")),
        PathBuf::from("deck.apkg.later.json")
    );
    assert_eq!(
        later_path(Path::new("no-extension")),
        PathBuf::from("no-extension.later.json")
    );
}

#[cfg(unix)]
#[test]
fn later_path_round_trips_non_utf8_name() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let name = OsStr::from_bytes(b"deck-\xff.csv");
    let deck_path = Path::new("/tmp").join(name);
    let result = later_path(&deck_path);
    assert_eq!(
        result.as_os_str().as_bytes(),
        b"/tmp/deck-\xff.csv.later.json"
    );
    // Round-trip: stripping the suffix gives back the original bytes.
    let bytes = result.as_os_str().as_bytes();
    assert_eq!(
        &bytes[..bytes.len() - b".later.json".len()],
        deck_path.as_os_str().as_bytes()
    );
}

#[test]
fn apkg_still_loads() {
    let dir = temp_dir("robustness-apkg-loads");
    let apkg_path = dir.join("one.apkg");
    let db = db_bytes(&dir, "one.anki2", &["Capital of France?\u{1f}Paris"]);
    write_apkg(&apkg_path, &[("collection.anki2", db)]);

    let deck = load_deck(&apkg_path).unwrap();
    assert_eq!(deck.cards.len(), 1);
    assert_eq!(deck.cards[0].front, "Capital of France?");
    assert_eq!(deck.cards[0].back, "Paris");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loading_two_apkgs_one_after_another_works() {
    let dir = temp_dir("robustness-apkg-twice");
    let first_path = dir.join("first.apkg");
    let first_db = db_bytes(&dir, "first.anki2", &["First front\u{1f}First back"]);
    write_apkg(&first_path, &[("collection.anki2", first_db)]);
    let second_path = dir.join("second.apkg");
    let second_db = db_bytes(
        &dir,
        "second.anki2",
        &["Second front\u{1f}Second back", "Extra\u{1f}Card"],
    );
    write_apkg(&second_path, &[("collection.anki2", second_db)]);

    let first = load_deck(&first_path).unwrap();
    assert_eq!(first.cards.len(), 1);
    assert_eq!(first.cards[0].front, "First front");

    let second = load_deck(&second_path).unwrap();
    assert_eq!(second.cards.len(), 2);
    assert_eq!(second.cards[0].front, "Second front");
    assert_eq!(second.cards[1].front, "Extra");

    // Loading the first deck again still yields its own cards.
    let first_again = load_deck(&first_path).unwrap();
    assert_eq!(first_again.cards.len(), 1);
    assert_eq!(first_again.cards[0].front, "First front");
    assert_eq!(first_again.cards[0].back, "First back");
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
