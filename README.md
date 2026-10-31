# Runki

[![CI](https://github.com/fhello23/runki/actions/workflows/ci.yml/badge.svg)](https://github.com/fhello23/runki/actions/workflows/ci.yml)

A minimal Anki deck viewer written in Rust. No spaced repetition, no
grading, no limits: open a deck and spacebar through it for as long as
you want. The only extra is a "study later" list.

## Run

```sh
cargo run --release            # then click "Open deck..."
cargo run --release -- path/to/deck.apkg
cargo run --release -- sample-deck.tsv
```

You can also drag a deck file onto the window.

## Keys

| Key / button | Action |
| --- | --- |
| `Space` | Show the answer; press again for the next card (wraps around at the end) |
| `L` | Toggle the current card in the study-later list |
| Top-right toggle | Switch between all cards and study-later only |

## Deck formats

- `.apkg` — real Anki decks, including modern exports. The newest
  collection found is used (`collection.anki21b`, then
  `collection.anki21`, then `collection.anki2`); the zstd-compressed
  `.anki21b` format from Anki 2.1.50+ is supported, so there is no need
  to re-export with "Support older Anki versions".
  A note's first field is the front; the remaining fields form the back.
- `.csv` / `.tsv` / `.txt` — one card per row: `front<separator>back`.
  Extra columns are appended to the back. A `front,back` header row is
  skipped.

## Study-later list

Flags are saved next to the deck in `<deck>.later.json` (card ids only),
so they survive restarts. Delete that file to clear the list.

## Limitations (by design, it is a plain viewer)

- Card HTML is shown as plain text; images appear as `[image]` and
  audio is ignored.
- Cloze deletions are shown with the answer revealed.
- Reversed cards generated from the same note are shown once.
- Cards are shown in deck order, looping forever. No shuffle, no stats.
