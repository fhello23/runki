use std::path::{Path, PathBuf};

use runki::deck::load_deck;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("runki-txt-header-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn load(tag: &str, file_name: &str, contents: &str) -> Vec<(String, String)> {
    let dir = temp_dir(tag);
    let path: &Path = &dir.join(file_name);
    std::fs::write(path, contents).unwrap();
    let deck = load_deck(path).unwrap();
    let cards = deck
        .cards
        .iter()
        .map(|c| (c.front.clone(), c.back.clone()))
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    cards
}

#[test]
fn anki_txt_example_with_tab_separator_header() {
    let cards = load(
        "example",
        "deck.txt",
        "#separator:tab\n#html:true\nQ1\tA1\nQ2, with comma\tA2\n",
    );
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2, with comma".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn anki_txt_semicolon_separator_header() {
    let cards = load(
        "semicolon",
        "deck.txt",
        "#separator:semicolon\n#html:true\nQ1;A1\nQ2, with comma;A2\n",
    );
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2, with comma".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn columns_header_with_tab_is_not_a_card() {
    let cards = load(
        "columns",
        "deck.txt",
        "#separator:tab\n#html:true\n#columns:Front\tBack\nQ1\tA1\n",
    );
    assert_eq!(cards, vec![("Q1".to_string(), "A1".to_string())]);
}

#[test]
fn txt_without_headers_tab() {
    let cards = load("no-header-tab", "deck.txt", "Q1\tA1\nQ2\tA2\n");
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn txt_without_headers_comma() {
    let cards = load("no-header-comma", "deck.txt", "Q1,A1\nQ2,A2\n");
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn hash_card_after_data_starts_is_kept() {
    let cards = load(
        "hash-card",
        "deck.txt",
        "#separator:tab\nQ1\tA1\n#1 hit song\tSome Artist\n",
    );
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("#1 hit song".to_string(), "Some Artist".to_string()),
        ]
    );
}

#[test]
fn front_back_header_row_after_anki_headers_is_skipped() {
    let cards = load(
        "front-back",
        "deck.txt",
        "#separator:tab\n#html:true\nFront\tBack\nQ1\tA1\n",
    );
    assert_eq!(cards, vec![("Q1".to_string(), "A1".to_string())]);
}

#[test]
fn csv_separator_header_overrides_extension() {
    let cards = load(
        "csv-override",
        "deck.csv",
        "#separator:semicolon\nQ1;A1\nQ2;A2\n",
    );
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn tsv_separator_literal_pipe_character() {
    let cards = load("tsv-pipe", "deck.tsv", "#separator:|\nQ1|A1\nQ2|A2\n");
    assert_eq!(
        cards,
        vec![
            ("Q1".to_string(), "A1".to_string()),
            ("Q2".to_string(), "A2".to_string()),
        ]
    );
}

#[test]
fn headerless_first_card_starting_with_hash_is_kept() {
    let cards = load("hash-first", "deck.csv", "#1 hit song,Some Artist\nQ2,A2\n");
    assert_eq!(
        cards,
        vec![
            ("#1 hit song".to_string(), "Some Artist".to_string()),
            ("Q2".to_string(), "A2".to_string()),
        ]
    );
}
