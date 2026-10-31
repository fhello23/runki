use std::io::{Cursor, Write};
use std::path::PathBuf;

use runki::deck::{clean_html, load_deck, load_later, parse_delimited, save_later};

#[test]
fn clean_html_strips_markup() {
    assert_eq!(
        clean_html("<b>Bold</b> &amp; <i>italic</i><br>next&nbsp;line"),
        "Bold & italic\nnext line"
    );
    assert_eq!(clean_html("<div>one</div><div>two</div>"), "one\ntwo");
    assert_eq!(
        clean_html("capital is {{c1::Paris::city}}"),
        "capital is Paris"
    );
    assert_eq!(clean_html("listen [sound:fr.mp3] now"), "listen  now");
    assert_eq!(clean_html("a &lt; b &#38;&#x26; c"), "a < b && c");
    assert_eq!(clean_html("<img src=\"x.png\">"), "[image]");
}

#[test]
fn parse_tsv_and_csv() {
    let cards = parse_delimited(b"front\tback\nQ1\tA1\nQ2\tA2\textra\n", b'\t').unwrap();
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].front, "Q1");
    assert_eq!(cards[1].back, "A2\n\nextra");
    assert_ne!(cards[0].id, cards[1].id);

    let cards = parse_delimited(b"\"Q, one\",A1\nQ2,A2\n", b',').unwrap();
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].front, "Q, one");
}

#[test]
fn later_list_roundtrips() {
    let dir = temp_dir("later");
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();
    let mut later = std::collections::HashSet::new();
    later.insert("abc".to_string());
    later.insert("123".to_string());
    save_later(&deck_path, &later).unwrap();
    assert_eq!(load_later(&deck_path), later);
    assert!(dir.join("deck.tsv.later.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loads_minimal_apkg() {
    let dir = temp_dir("apkg");
    let db_path = dir.join("collection.anki2");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, flds TEXT);
             CREATE TABLE cards (id INTEGER PRIMARY KEY, nid INTEGER);
             INSERT INTO notes VALUES (1, '<b>Capital of France?</b>\u{1f}Paris');
             INSERT INTO notes VALUES (2, 'Water formula\u{1f}H2O\u{1f}extra note');
             INSERT INTO cards VALUES (10, 1);
             INSERT INTO cards VALUES (11, 1);
             INSERT INTO cards VALUES (12, 2);",
        )
        .unwrap();
    }
    let db_bytes = std::fs::read(&db_path).unwrap();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("collection.anki2", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&db_bytes).unwrap();
    let apkg_bytes = zip.finish().unwrap().into_inner();

    let apkg_path = dir.join("test.apkg");
    std::fs::write(&apkg_path, apkg_bytes).unwrap();
    let deck = load_deck(&apkg_path).unwrap();
    // Card 11 is the same note as card 10 (e.g. reversed): shown once.
    assert_eq!(deck.cards.len(), 2);
    assert_eq!(deck.cards[0].id, "10");
    assert_eq!(deck.cards[0].front, "Capital of France?");
    assert_eq!(deck.cards[0].back, "Paris");
    assert_eq!(deck.cards[1].back, "H2O\n\nextra note");
    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("runki-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
