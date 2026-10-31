<div align="center">

<img src="assets/icon.png" alt="Runki icon" width="128">

# Runki

[![CI](https://github.com/fhello23/runki/actions/workflows/ci.yml/badge.svg)](https://github.com/fhello23/runki/actions/workflows/ci.yml)

</div>

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
| **Order:** `Deck order` / `Shuffled` | Show cards as they are in the deck, or in a shuffled order |
| **Reshuffle** (shuffled only) | Pick a new shuffle; the card on screen stays |
| **Show:** `All` / `Unseen` / `Study later (n)` | Show every card, only cards you have not seen yet, or only flagged cards |

Revealing a card's answer marks it seen. In `Unseen`, the card stays
on screen until you move on. When every card has been seen, `Unseen`
offers **Reset progress** (with a confirmation): seen marks are
cleared, study-later flags are kept. The top bar shows how many cards
you have seen (`seen / total`); the bottom bar shows the current
card's place in the shown list.

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

## Progress

Study-later flags, which cards you have seen (a card becomes seen
when its answer is revealed), and where you left off — the current
card, order, filter, and shuffle — are saved next to the deck in
`<deck>.runki.json`, so they survive restarts and sync with the deck.
Reopening a deck resumes at the remembered card.
Delete that file to clear a deck's progress. A legacy
`<deck>.later.json` from older versions is imported once into the new
file and left in place. Editing a CSV row or renaming a deck file
loses that card's / deck's progress, because progress is keyed by
card id and deck file name.

## Limitations (by design, it is a plain viewer)

- Card HTML is shown as plain text; images appear as `[image]` and
  audio is ignored.
- Cloze deletions are shown with the answer revealed.
- Reversed cards generated from the same note are shown once.
- Cards loop forever in the chosen order. "Seen" is only a checkmark:
  no scheduling, grading, or stats beyond the seen count.
