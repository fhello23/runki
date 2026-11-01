# Changelog

All notable changes to Runki are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/) (while Runki is `0.x`,
a minor bump may change behaviour).

Add a line under **Unreleased** in every PR that changes what users
see. Releasing moves those lines under the new version (see
"Releasing" in the README).

## [Unreleased]

## [0.4.1] - 2026-11-01

### Fixed

- The study progress bar now shows the share of the deck seen
  (`seen / total`) instead of the card's place in the shown list,
  which on a resumed shuffle could look like progress not yet made.
  The place in the list is still shown as text below the bar.

## [0.4.0] - 2026-10-31

### Added

- Optional LAN web study interface, controlled from both desktop screens,
  the macOS Runki menu, or TUI `w`, with an editable remembered port and
  temporary `--web` / `--no-web` / `--web-port` overrides.
- Browser library and full study controls sharing the existing progress files;
  no login, so anyone on the network can study and change progress.

## [0.3.5] - 2026-10-31

### Added

- A full terminal interface (`runki --tui`): library browsing, adding
  decks, folder selection, every study control, scrollable cards,
  light/dark themes and in-app help. Shares library settings and
  progress with the desktop app; existing settings import automatically.
- `--library DIR` to select a library from the command line, and
  `--no-default-features` builds for terminal use without desktop dependencies.

## [0.3.0] - 2026-10-31

### Changed

- A minimal library and study interface, centred cards, clickable
  keyboard hints, and a top-right theme control for system, light or
  dark appearance. The theme choice is remembered between launches.

## [0.2.0] - 2026-10-31

The first published release. Builds before this one all called
themselves 0.1.0 and were never released.

### Added

- Versioning and releases: `runki --version` and `runki --help`, the
  version on the library and start screens, this changelog, and
  downloads on GitHub Releases (`Runki.app` for macOS, a binary for
  Linux).
- Deck library: a folder of decks listed with card count, seen
  progress, study-later count and when each was last opened. **Add
  deck…** copies decks in without overwriting. Pick any folder, e.g.
  one in iCloud Drive or Dropbox to sync between Macs.
- Safe syncing between Macs: study-later flags and seen marks merge
  per card, sync conflict copies of progress are merged and removed,
  and online-only decks show as "Not downloaded" instead of blocking.
- Study session: deck order or shuffled (with **Reshuffle**), show
  all / unseen / study-later cards, resume at the last card, and
  **Reset progress**.
- Per-deck progress (seen marks, study-later flags, position) saved
  with the deck; an old `<deck>.later.json` is imported once.
- The macOS **About Runki** panel shows the icon, version and licence.
- An app icon.
- GitHub Actions CI (format, Clippy and tests on macOS and Linux).

### Fixed

- Study-later flags are never lost to a corrupt or half-written file.
- Modern `.apkg` exports load their real collection instead of the
  "requires a newer version" placeholder.
- Anki plain-text headers (`#separator:`, `#html:`, …) are honoured
  when loading `.txt` / `.csv` / `.tsv` decks.
- A bare `<` or `&` in card text no longer swallows the text after it.
- Only `{{c<digits>::` is treated as a cloze.

## 0.1.0

The first, unreleased version: open an `.apkg`, `.csv`, `.tsv` or
`.txt` deck, spacebar through it, and flag cards to study later.

[Unreleased]: https://github.com/fhello23/runki/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/fhello23/runki/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/fhello23/runki/compare/v0.3.5...v0.4.0
[0.3.5]: https://github.com/fhello23/runki/compare/v0.3.0...v0.3.5
[0.3.0]: https://github.com/fhello23/runki/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/fhello23/runki/releases/tag/v0.2.0
