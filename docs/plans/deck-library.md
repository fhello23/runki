# Plan: deck library, progress, shuffle and "unseen only"

Status: approved, being implemented phase by phase (one PR each,
stacked). Prerequisites #7–#11 are merged.

## Goal

Open Runki and see a list of your decks from one folder that syncs between
Macs (Dropbox, iCloud Drive, or any local folder). Pick a deck and study it
in deck order or shuffled, optionally showing only cards you haven't seen
yet. Progress is saved in the same folder, so it follows you to every Mac,
and can be reset per deck.

Still no spaced repetition and no grading: "seen" is just a checkmark.

## Decisions (former open questions)

1. A card becomes **seen when its answer is revealed** (first Space).
2. Flat library folder, no subfolders, in v1.
3. Reset is per deck only.
4. *Add deck* **copies** the file into the library.
5. New decks default to `All`; each deck remembers its own filter.

## User-facing behaviour

### Library screen (new start screen)
- First launch: "Choose a library folder…" (folder picker, with New
  Folder enabled) and a hint that any folder works, iCloud Drive or
  Dropbox included. No folders are suggested: the user always picks.
  The folder can be changed later. If the folder is
  missing/unreadable, say so and offer to choose another.
- One row per deck file (`.apkg`, `.csv`, `.tsv`, `.txt`, case-insensitive)
  directly in the folder: name, card count, progress bar `seen / total`,
  study-later count, last opened. Sorted by last opened (newest first),
  then name. Dotfiles, `.runki/`, `*.later.json`, `*.runki.json` and
  `*.tmp` are not listed.
- Rows appear immediately; counts fill in from a background loader.
  A deck that fails to parse stays listed with its error (e.g. a stray
  notes `.txt`).
- Files not downloaded yet (iCloud/Dropbox online-only) are listed greyed
  out: "Not downloaded – open in Finder to download". They are never read
  by the background loader (reading would block on a download).
- **Add deck…** button and drag-and-drop onto the library screen *copy*
  the file in. Name clash → `name (2).apkg`, `name (3).apkg`, …. Copy to
  a hidden temp file in the library, then rename, so sync clients and the
  scanner never see a partial file. Existing progress for the source deck
  (`<deck>.runki.json`, else legacy `<deck>.later.json`) is carried over
  into the new library state file.
- Clicking a row opens the deck in the study screen.
- Decks opened from outside the library (CLI argument, drop on the study
  screen, Open deck…) still work, with progress in a sidecar next to the
  file. A deck whose canonical parent directory *is* the library uses the
  library state, however it was opened.

### Study screen (today's screen, extended)
- **Library** button in the top bar goes back (saves state first).
- **Order:** `Deck order` | `Shuffled`. **Reshuffle** picks a new seed.
- **Show:** `All` | `Unseen` | `Study later` (replaces All / Study later).
  Order and filter are independent.
- Revealing the answer marks the card seen.
- In `Unseen`, the current card stays on screen after being revealed;
  it drops out when you advance. In `Study later`, unflagging behaves as
  today (PR #1): the card leaves the view at once, the next card is shown
  with its answer hidden.
- When the filtered view is empty in `Unseen`: "You've seen all N cards"
  with **Reset progress** (confirmation dialog). Reset clears *seen*
  only; study-later flags are kept.
- The current card, order, filter and seed are remembered per deck, so
  reopening resumes where you left off.
- Top bar shows `seen / total` for the deck; the bottom bar keeps
  `n / view size`.

## Data model

### App config (per Mac, not synced)
eframe's `persistence` feature (`cc.storage` in the creator closure,
`App::save`, an `app_id`): library folder path, last open deck, window
size (eframe does this itself). A CLI argument wins over "last open deck".

### Deck state (synced)
Library decks: `<library>/.runki/<deck file name>.json`.
Other decks: sidecar `<deck file name>.runki.json` next to the deck.

```json
{
  "version": 1,
  "later": { "1700000000123": { "on": true, "t": 1730390000123 } },
  "seen": { "1700000000123": 1730390000456 },
  "reset_at": 0,
  "prefs": {
    "t": 1730390000789,
    "order": "shuffled",
    "filter": "unseen",
    "shuffle_seed": 8811234,
    "position": "9f2c…"
  },
  "opened_at": 1730390000000
}
```

All times are unix **milliseconds**.

- `later`: per card a last-writer-wins register. Unflagging writes
  `on: false` with a newer `t`; it is not deleted. A card is flagged iff
  its entry is `on`.
- `seen`: card id → time it was last seen. A card is seen iff its time
  is `> reset_at`. Reset sets `reset_at = now` and leaves `seen` alone.
- `prefs`: order/filter/seed/current card as one last-writer-wins unit
  (`t` = when any of them last changed). `position` is a card id, so it
  survives reshuffles, filter changes and deck edits.
- `opened_at`: last time the deck was opened on any Mac (max-merged).
- Counts shown anywhere (`seen / total`, study-later count) only count
  ids present in the loaded deck; stale ids are kept in the file.

### Merge (pure function, used on save and for conflict copies)
`merge(a, b)`: `later` per id: entry with larger `t` (tie → `on: true`);
`seen` per id: max; `reset_at`: max; `prefs`: the one with larger `t`
(tie → `a`); `opened_at`: max. Merge is commutative/associative/idempotent
except for the documented `prefs` tie rule; test that.

### Load / save rules
- Missing file → empty state (then try migration).
- Unreadable file, invalid JSON, or `version` > 1 → error shown, state
  kept in memory, **saving blocked** for that deck until reopened (same
  as `later_save_blocked` today). Never overwrite a file we can't read.
- **Migration:** no state file but `<deck>.later.json` exists and parses
  → import ids as `{on: true, t: 0}`. Keep the old file. If the legacy
  file is unreadable, open with empty state and block saving (as today).
- **Save = read disk, merge, write atomically:** re-read the file; if it
  is unreadable/invalid/newer → don't write, report the error. Otherwise
  write `merge(in_memory, disk)` to a hidden temp file in the same
  directory (`.<name>.<pid>.tmp`), `sync_all`, rename over. Adopt the
  merged result in memory.
- **Debounced:** state changes mark the session dirty; the app saves at
  most every ~2 s while dirty, and always on deck switch, going back to
  the library, and app exit (`App::save` / `on_exit`).

### Order
Shuffled order = cards sorted by `(splitmix64(seed ^ fnv1a(card.id)),
deck index)`. In-tree, no RNG crate: identical on every Mac and app
version, adding/removing cards doesn't reorder the rest, and a reshuffle
keeps the current card (it is tracked by id). New seeds come from the
clock mixed through splitmix64.

### Card ids
Unchanged: Anki card id for `.apkg`, content hash for CSV/TSV/TXT.
Editing a CSV row or renaming a deck file therefore loses that card's /
deck's progress; state this in the README.

## Session semantics (`src/session.rs`, pure, no egui)
`StudySession` holds the deck, the `DeckState`, `current: Option<card id>`,
`revealed`, `dirty`, `save_blocked`.

- `order()` = all card indices in deck order or shuffled order.
- `view()` = cards in `order()` passing the filter, **plus the current
  card** if it fails the filter only because it was seen this session
  (Unseen) — so the shown card never vanishes on reveal.
- `space()`: not revealed → reveal + mark seen. Revealed → move to the
  next card after the current one in `order()` that passes the filter
  (wrapping), hide answer. If none passes → `current = None` (empty view).
- `toggle_later()`: as today, incl. the PR #1 behaviour in `Study later`.
- `set_order`, `set_filter`, `reshuffle`: keep the current card if it is
  in the new view, else move to the next card after it in the new order
  that passes the filter; hide the answer if the card changed.
- `reset()`: `reset_at = now`; in `Unseen` jump to the first card.
- Resume on open: `prefs.position` if present and in view; else the next
  card after it (in order) passing the filter; else the first in view.
- Counters: `seen_count()`, `later_count()`, `(n, view_len)` for the
  current card.

## Sync safety (Dropbox / iCloud)
- Atomic writes + merge-on-save (above) mean two Macs never lose each
  other's flags or seen marks, as long as one Mac's file has synced
  before the other saves.
- **Conflict copies** in `.runki/`: a `.json` file that is not exactly
  `<existing deck file name>.json` but starts with a deck's state-file
  stem and looks like a conflict copy (`… (… conflicted copy …).json`,
  `<deck file name> 2.json`, `<deck file name>.json 2` patterns). Merge it
  into that deck's state, save, then delete the copy. Never touch a file
  that is the exact state-file name of some deck (e.g. a deck really
  called `French 2.apkg`). Unparseable copies are left alone.
- **Not-downloaded files:** on macOS, `st_flags & SF_DATALESS`
  (`0x40000000`, via `std::os::macos::fs::MetadataExt::st_flags`), plus
  legacy `.<name>.icloud` placeholders (pre-Sonoma iCloud). Shown greyed,
  never read in the background.

## Code structure

| File | Change |
| --- | --- |
| `src/progress.rs` (new) | `DeckState` (serde), merge, load/save rules, migration, state-path resolution. No egui. |
| `src/session.rs` (new) | `StudySession` as above. No egui. |
| `src/library.rs` (new) | Folder scan, row model, not-downloaded detection, add-deck copy, background loader (thread + channel + `ctx.request_repaint()` callback), conflict-copy handling. No egui types in its API. |
| `src/app.rs` | `Screen::{Library, Study}`, library UI, study UI on top of `StudySession`, config persistence, debounced saves. |
| `src/deck.rs` | Remove `later_path`/`load_later`/`save_later` once `progress.rs` replaces them (keep a private legacy reader for migration in `progress.rs`). |
| `Cargo.toml` | `serde` with `derive`; eframe `persistence` feature. Nothing else. |

## Implementation phases (one PR each, stacked)

1. **Progress model** — `progress.rs` complete (state, merge, load/save
   rules, migration, sidecar path; library path resolution API accepting
   an optional library dir), app uses it instead of `.later.json`
   (sidecar `.runki.json`), mark seen on reveal, debounced save + save on
   exit. UI otherwise unchanged. Tests.
2. **Study session** — `session.rs`; order/filter/reshuffle/reset with
   confirm/resume; `seen / total`; app study UI uses it. Tests for view
   and position logic (Unseen keeps current card until advance, wrap,
   reshuffle keeps card, resume fallbacks, Study-later unflag).
3. **Library** — config persistence, `library.rs` scan + background
   loader, library screen, open/back, add deck (button + drop = copy,
   with progress carry-over), library-vs-sidecar state resolution,
   `opened_at`, README.
4. **Sync hardening** — conflict-copy merging, not-downloaded rows.

Each phase keeps `cargo fmt --check`, `cargo clippy --all-targets --locked
-- -D warnings` and `cargo test --locked` green.
