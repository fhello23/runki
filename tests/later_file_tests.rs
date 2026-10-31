use std::collections::HashSet;
use std::path::PathBuf;

use runki::deck::{later_path, load_later, save_later};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("runki-test-later-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn missing_file_gives_ok_empty() {
    let dir = temp_dir("missing");
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();

    let later = load_later(&deck_path).expect("missing file is Ok");
    assert!(later.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_json_gives_err() {
    let dir = temp_dir("corrupt");
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();
    std::fs::write(later_path(&deck_path), "not valid json {{{").unwrap();

    let err = load_later(&deck_path).expect_err("corrupt JSON is Err");
    assert!(
        format!("{err:#}").contains("deck.tsv.later.json"),
        "error should name the file: {err:#}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_then_load_roundtrips() {
    let dir = temp_dir("roundtrip");
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();
    let mut later = HashSet::new();
    later.insert("abc".to_string());
    later.insert("123".to_string());

    save_later(&deck_path, &later).unwrap();
    assert_eq!(load_later(&deck_path).unwrap(), later);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_leaves_no_tmp_file() {
    let dir = temp_dir("no-tmp");
    let deck_path = dir.join("deck.tsv");
    std::fs::write(&deck_path, "Q\tA\n").unwrap();
    let mut later = HashSet::new();
    later.insert("abc".to_string());

    save_later(&deck_path, &later).unwrap();

    assert!(later_path(&deck_path).exists());
    assert!(!dir.join("deck.tsv.later.json.tmp").exists());
    let tmp_left = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!tmp_left);

    let _ = std::fs::remove_dir_all(&dir);
}
