use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(all(feature = "gui", target_os = "macos"))]
mod about_macos;

use runki::config;
#[cfg(feature = "gui")]
use runki::config::Config;
use runki::VERSION;

const USAGE: &str = "\
Usage: runki [OPTIONS] [DECK]

Opens the library, or DECK (.apkg, .csv, .tsv, .txt) straight away.
The desktop interface is the default; terminal-only builds use the TUI.

Options:
  -t, --tui          Run the full terminal interface
      --library DIR Use this library folder (remembered for both interfaces)
  -h, --help         Print this help
  -V, --version      Print the version
      --            Treat remaining arguments as a deck path

In the TUI, press ? for keys and q to save and quit.";

#[derive(Debug, PartialEq)]
enum Command {
    Run {
        tui: bool,
        deck: Option<PathBuf>,
        library: Option<PathBuf>,
    },
    Version,
    Help,
}

fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args;
    let mut tui = !cfg!(feature = "gui");
    let mut deck = None;
    let mut library = None;
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional {
            match arg.to_str() {
                Some("-V" | "--version") => return Ok(Command::Version),
                Some("-h" | "--help") => return Ok(Command::Help),
                Some("-t" | "--tui") => {
                    tui = true;
                    continue;
                }
                Some("--library") => {
                    if library.is_some() {
                        return Err("--library was given twice".into());
                    }
                    let dir = args.next().ok_or("--library needs a folder path")?;
                    if dir.is_empty() || dir.to_string_lossy().starts_with('-') {
                        return Err(
                            "--library needs a folder path (prefix a leading '-' with ./)".into(),
                        );
                    }
                    library = Some(PathBuf::from(dir));
                    continue;
                }
                Some("--") => {
                    positional = true;
                    continue;
                }
                Some(psn) if psn.starts_with("-psn_") => continue,
                Some(flag) if flag.starts_with('-') && flag != "-" => {
                    return Err(format!("unknown option {flag}"))
                }
                _ => {}
            }
        }
        if deck.is_some() {
            return Err("only one deck path can be opened at a time".into());
        }
        deck = Some(PathBuf::from(arg));
    }
    Ok(Command::Run { tui, deck, library })
}

fn main() -> ExitCode {
    let command = match parse_args(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("runki: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (tui, deck, library) = match command {
        Command::Run { tui, deck, library } => (tui, deck, library),
        Command::Version => {
            println!("runki {VERSION}");
            return ExitCode::SUCCESS;
        }
        Command::Help => {
            println!("runki {VERSION}\n\n{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    match run(tui, deck, library) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runki: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(tui: bool, deck: Option<PathBuf>, library: Option<PathBuf>) -> anyhow::Result<()> {
    if let Some(dir) = &library {
        anyhow::ensure!(
            dir.is_dir(),
            "Library folder {} does not exist or is not a directory",
            dir.display()
        );
    }
    let library = library.map(std::fs::canonicalize).transpose()?;
    let path = config::file_path()?;
    let shared_config = match config::load(&path)? {
        Some(config) => Some(config),
        None => config::load_legacy()?,
    };
    if tui {
        let mut config = shared_config.unwrap_or_default();
        if let Some(dir) = library {
            config.library = Some(dir);
        }
        return runki::tui::run(config, path, deck);
    }
    #[cfg(feature = "gui")]
    run_gui(deck, shared_config, library, path).map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

#[cfg(feature = "gui")]
fn run_gui(
    initial_deck: Option<PathBuf>,
    shared_config: Option<Config>,
    library: Option<PathBuf>,
    config_path: PathBuf,
) -> eframe::Result<()> {
    use runki::app::{RunkiApp, CONFIG_KEY};
    use runki::theme;
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_app_id("runki")
        .with_inner_size([900.0, 650.0])
        .with_min_inner_size([520.0, 400.0])
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                .expect("assets/icon.png is a valid PNG"),
        );
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Runki",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            about_macos::install();
            theme::install(&cc.egui_ctx);
            let choice = theme::load(cc.storage);
            cc.egui_ctx.set_theme(choice);
            // A CLI deck (initial_deck) wins over the saved config:
            // it alone opens straight into the study screen.
            let mut config: Config = shared_config.unwrap_or_else(|| {
                cc.storage
                    .and_then(|storage| eframe::get_value(storage, CONFIG_KEY))
                    .unwrap_or_default()
            });
            if let Some(dir) = library {
                config.library = Some(dir);
            }
            Ok(Box::new(
                RunkiApp::new(config, initial_deck)
                    .with_theme(choice)
                    .with_config_path(config_path),
            ))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn defaults_and_finder_launch() {
        let expected = Command::Run {
            tui: !cfg!(feature = "gui"),
            deck: None,
            library: None,
        };
        assert_eq!(parse(&[]), Ok(expected));
        assert_eq!(parse(&["-psn_0_12345"]), parse(&[]));
    }

    #[test]
    fn paths_and_flags_work_in_either_order() {
        let expected = Command::Run {
            tui: true,
            deck: Some("deck.apkg".into()),
            library: Some("my decks".into()),
        };
        assert_eq!(
            parse(&["--tui", "--library", "my decks", "deck.apkg"]),
            Ok(expected)
        );
        assert_eq!(
            parse(&["deck.apkg", "--tui", "--library", "my decks"]),
            parse(&["--tui", "--library", "my decks", "deck.apkg"])
        );
        assert!(
            matches!(parse(&["--tui", "--", "-deck.tsv"]), Ok(Command::Run { deck: Some(p), .. }) if p == std::path::Path::new("-deck.tsv"))
        );
    }

    #[test]
    fn version_help_and_invalid_arguments() {
        for arg in ["--version", "-V"] {
            assert_eq!(parse(&[arg]), Ok(Command::Version));
        }
        for arg in ["--help", "-h"] {
            assert_eq!(parse(&[arg]), Ok(Command::Help));
        }
        for args in [
            &["--verbose"][..],
            &["--library"],
            &["a.tsv", "b.tsv"],
            &["--library", "--tui"],
            &["--library", "a", "--library", "b"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"deck-\xff.tsv".to_vec());
        assert!(
            matches!(parse_args([OsString::from("--tui"), path.clone()].into_iter()), Ok(Command::Run { deck: Some(p), .. }) if p == path)
        );
    }
}
