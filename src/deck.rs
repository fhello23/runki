//! Deck loading (`.apkg`, CSV, TSV) and study-later persistence.
//!
//! An `.apkg` is a zip containing a SQLite database
//! (`collection.anki21b`, `collection.anki21`, or `collection.anki2`;
//! the `.anki21b` variant is zstd-compressed). Cards are read from the
//! `cards`/`notes` tables: a note's first field is the front, the
//! remaining fields are the back. HTML is stripped down to plain text.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// Stable id used by the study-later list.
    pub id: String,
    pub front: String,
    pub back: String,
}

#[derive(Debug, Clone)]
pub struct Deck {
    pub name: String,
    pub path: PathBuf,
    pub cards: Vec<Card>,
}

/// Load a deck from `.apkg`, `.csv`, `.tsv`, or `.txt` (tab/comma sniffed).
pub fn load_deck(path: &Path) -> Result<Deck> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let cards = match ext.as_str() {
        "apkg" => load_apkg(path)?,
        "csv" | "tsv" | "txt" => {
            let bytes = std::fs::read(path)?;
            let (header_delim, body) = strip_anki_headers(&bytes);
            let delim = header_delim.unwrap_or_else(|| match ext.as_str() {
                "csv" => b',',
                "tsv" => b'\t',
                _ => {
                    let first_line = body
                        .split(|&b| b == b'\n')
                        .find(|line| !line.trim_ascii().is_empty())
                        .unwrap_or(&[]);
                    if first_line.contains(&b'\t') {
                        b'\t'
                    } else {
                        b','
                    }
                }
            });
            parse_delimited(body, delim)?
        }
        other => {
            return Err(anyhow!(
                "Unsupported file type '.{other}'. Open an .apkg, .csv, .tsv, or .txt deck."
            ))
        }
    };
    if cards.is_empty() {
        return Err(anyhow!("No cards found in {}", path.display()));
    }
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("deck")
        .to_string();
    Ok(Deck {
        name,
        path: path.to_path_buf(),
        cards,
    })
}

fn load_apkg(path: &Path) -> Result<Vec<Card>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).context("Not a valid .apkg (zip) file")?;

    // Newest first: modern Anki also ships a dummy `collection.anki2`
    // holding a "requires a newer version" placeholder note, so it must
    // only be used when nothing newer is present.
    let mut db_bytes = None;
    for entry in [
        "collection.anki21b",
        "collection.anki21",
        "collection.anki2",
    ] {
        if let Ok(mut zipped) = archive.by_name(entry) {
            let mut buf = Vec::new();
            zipped.read_to_end(&mut buf)?;
            db_bytes = Some(if entry == "collection.anki21b" {
                zstd::decode_all(&buf[..])
                    .context("Could not decompress collection.anki21b inside this .apkg")?
            } else {
                buf
            });
            break;
        }
    }
    let db_bytes = db_bytes.ok_or_else(|| {
        anyhow!(
            "No collection.anki21b, collection.anki21, or \
             collection.anki2 inside this .apkg."
        )
    })?;

    // rusqlite needs a real file, so park the database in a temp file.
    let tmp = std::env::temp_dir().join(format!("runki-{}.anki2", std::process::id()));
    std::fs::write(&tmp, &db_bytes)?;
    let result = read_anki_db(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn read_anki_db(db_path: &Path) -> Result<Vec<Card>> {
    let conn = rusqlite::Connection::open(db_path)
        .context("Could not open the Anki database inside the .apkg")?;
    let mut stmt = conn.prepare(
        "SELECT c.id, c.nid, n.flds FROM cards c \
         JOIN notes n ON n.id = c.nid ORDER BY c.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut cards = Vec::new();
    let mut seen_notes = HashSet::new();
    for row in rows {
        let (card_id, note_id, flds) = row?;
        // One note can generate several cards (e.g. reversed); this is a
        // plain viewer, so show each note once.
        if !seen_notes.insert(note_id) {
            continue;
        }
        let fields: Vec<&str> = flds.split('\u{1f}').collect();
        let front = fields.first().map(|f| clean_html(f)).unwrap_or_default();
        let back = fields[1..]
            .iter()
            .map(|f| clean_html(f))
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if front.is_empty() && back.is_empty() {
            continue;
        }
        cards.push(Card {
            id: card_id.to_string(),
            front,
            back,
        });
    }
    Ok(cards)
}

/// Header keys Anki writes at the top of its plain-text exports.
const ANKI_HEADER_KEYS: &[&str] = &[
    "separator",
    "html",
    "tags",
    "columns",
    "notetype",
    "deck",
    "notetype column",
    "deck column",
    "tags column",
    "guid column",
];

/// Strip a leading block of Anki `#` header lines, returning any
/// `#separator:` delimiter it declares and the remaining bytes.
/// Only lines at the very top are headers; a `#` after data is a card.
fn strip_anki_headers(bytes: &[u8]) -> (Option<u8>, &[u8]) {
    let mut delim = None;
    let mut offset = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    while offset < bytes.len() {
        let line_end = bytes[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| offset + p);
        let line = bytes[offset..line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&bytes[offset..line_end]);
        // Only Anki's own `#key:value` lines are headers, so a first card
        // such as "#1 hit song" is kept.
        let Some((key, value)) = std::str::from_utf8(line)
            .ok()
            .and_then(|text| text.strip_prefix('#'))
            .and_then(|text| text.split_once(':'))
        else {
            break;
        };
        let key = key.trim().to_ascii_lowercase();
        if !ANKI_HEADER_KEYS.contains(&key.as_str()) {
            break;
        }
        if key == "separator" && delim.is_none() {
            delim = parse_separator_value(value.trim());
        }
        offset = if line_end < bytes.len() {
            line_end + 1
        } else {
            bytes.len()
        };
    }
    (delim, &bytes[offset..])
}

fn parse_separator_value(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "tab" => Some(b'\t'),
        "comma" => Some(b','),
        "semicolon" => Some(b';'),
        "space" => Some(b' '),
        "pipe" => Some(b'|'),
        "colon" => Some(b':'),
        _ => {
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii() => Some(c as u8),
                _ => None,
            }
        }
    }
}

/// Parse `front<delim>back` records (extra columns join the back).
/// A leading `front,back` style header row is skipped.
pub fn parse_delimited(bytes: &[u8], delimiter: u8) -> Result<Vec<Card>> {
    let (header_delim, bytes) = strip_anki_headers(bytes);
    let delimiter = header_delim.unwrap_or(delimiter);
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(bytes);
    let mut cards = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (i, record) in reader.records().enumerate() {
        let record = record.context("Could not parse deck file")?;
        if record.len() < 2 {
            continue;
        }
        let front = clean_html(record[0].trim());
        let back = record
            .iter()
            .skip(1)
            .map(|f| clean_html(f.trim()))
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if i == 0
            && front.eq_ignore_ascii_case("front")
            && back.eq_ignore_ascii_case("back")
        {
            continue;
        }
        if front.is_empty() && back.is_empty() {
            continue;
        }
        // Content hash as id, so study-later flags survive reordering.
        let base = fnv1a(&format!("{front}\u{1f}{back}"));
        let mut id = format!("{base:016x}");
        let mut n = 0;
        while !seen_ids.insert(id.clone()) {
            n += 1;
            id = format!("{base:016x}-{n}");
        }
        cards.push(Card { id, front, back });
    }
    Ok(cards)
}

/// Path of the study-later sidecar file for a deck.
pub fn later_path(deck_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.later.json", deck_path.display()))
}

/// Load flagged card ids for a deck; empty set if none saved yet.
pub fn load_later(deck_path: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(later_path(deck_path)) else {
        return HashSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Save flagged card ids next to the deck file.
pub fn save_later(deck_path: &Path, later: &HashSet<String>) -> Result<()> {
    let mut ids: Vec<&String> = later.iter().collect();
    ids.sort();
    let text = serde_json::to_string_pretty(&ids)?;
    std::fs::write(later_path(deck_path), text).with_context(|| {
        format!(
            "Could not save study-later list next to {}",
            deck_path.display()
        )
    })?;
    Ok(())
}

/// Stable FNV-1a hash (the std hasher is not stable across runs/releases).
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Turn Anki field HTML into readable plain text.
pub fn clean_html(input: &str) -> String {
    let text = reveal_cloze(input);
    let text = strip_sound(&text);
    let text = strip_tags(&text);
    let text = decode_entities(&text);
    tidy(&text)
}

/// `{{c1::answer::hint}}` -> `answer` (this viewer has no cloze mode).
fn reveal_cloze(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{c") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        // Only `{{c<digits>::` is a cloze opener; other `{{c` text (e.g.
        // `{{curly}}`) is emitted literally.
        let digits = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 || !after[digits..].starts_with("::") {
            out.push_str("{{c");
            rest = after;
            continue;
        }
        let content = &after[digits + 2..];
        let end = content.find("}}").unwrap_or(content.len());
        let body = &content[..end];
        out.push_str(body.split("::").next().unwrap_or(body));
        rest = &content[end + 2.min(content.len() - end)..];
    }
    out.push_str(rest);
    out
}

/// Drop `[sound:file.mp3]` markers.
fn strip_sound(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("[sound:") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(']') {
            Some(end) => rest = &after[end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip tags, keeping line structure (`<br>`, block ends -> newlines,
/// `<img>` -> `[image]`).
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '<' {
            out.push(ch);
            continue;
        }
        let is_tag_start = matches!(
            chars.peek(),
            Some(c) if c.is_ascii_alphabetic() || *c == '/' || *c == '!'
        );
        if !is_tag_start {
            out.push('<');
            continue;
        }
        let mut tag = String::new();
        let mut closed = false;
        for tc in chars.by_ref() {
            if tc == '>' {
                closed = true;
                break;
            }
            tag.push(tc);
        }
        if !closed {
            out.push('<');
            out.push_str(&tag);
            break;
        }
        let tag = tag.to_ascii_lowercase();
        let name = tag
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        match name {
            "br" => out.push('\n'),
            "img" => out.push_str("[image]"),
            "div" | "p" | "li" | "tr" | "h1" | "h2" | "h3" | "h4"
                if tag.starts_with('/') =>
            {
                out.push('\n')
            }
            _ => {}
        }
    }
    out
}

/// Decode the handful of HTML entities Anki fields actually contain.
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi];
        let name = entity.strip_prefix('#').unwrap_or(entity);
        if entity.len() > 32
            || name.is_empty()
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            out.push('&');
            rest = after;
            continue;
        }
        let decoded = match entity {
            "nbsp" => Some(" ".to_string()),
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#')?.parse::<u32>().ok())
                .and_then(char::from_u32)
                .map(|c| c.to_string()),
        };
        match decoded {
            Some(text) => out.push_str(&text),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Trim lines and collapse runs of blank lines to a single blank line.
fn tidy(input: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in input.lines().map(str::trim) {
        if line.is_empty() && (lines.is_empty() || lines.last() == Some(&"")) {
            continue;
        }
        lines.push(line);
    }
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines.join("\n")
}

