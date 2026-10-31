use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "macos")]
mod about_macos;

use runki::app::{Config, RunkiApp, CONFIG_KEY};
use runki::VERSION;

const USAGE: &str = "\
Usage: runki [DECK]

Opens the library, or DECK (.apkg, .csv, .tsv, .txt) straight away.

Options:
  -h, --help     Print this help
  -V, --version  Print the version";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Command {
    Run(Option<PathBuf>),
    Version,
    Help,
}

/// Reads the arguments after the program name. The first one decides:
/// a flag, or a deck path. Anything after it is ignored, as before.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let Some(first) = args.next() else {
        return Ok(Command::Run(None));
    };
    match first.as_str() {
        "-V" | "--version" => Ok(Command::Version),
        "-h" | "--help" => Ok(Command::Help),
        // Old macOS passes a process serial number to apps opened from
        // Finder; it is not a deck.
        psn if psn.starts_with("-psn_") => Ok(Command::Run(None)),
        flag if flag.starts_with('-') && flag != "-" => Err(format!("unknown option {flag}")),
        _ => Ok(Command::Run(Some(PathBuf::from(first)))),
    }
}

fn main() -> ExitCode {
    let initial_deck = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Run(deck)) => deck,
        Ok(Command::Version) => {
            println!("runki {VERSION}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Help) => {
            println!("runki {VERSION}\n\n{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("runki: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(initial_deck) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runki: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(initial_deck: Option<PathBuf>) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_app_id("runki")
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([520.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .expect("assets/icon.png is a valid PNG"),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "Runki",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            about_macos::install();
            // A CLI deck (initial_deck) wins over the saved config:
            // it alone opens straight into the study screen.
            let config: Config = cc
                .storage
                .and_then(|storage| eframe::get_value(storage, CONFIG_KEY))
                .unwrap_or_default();
            Ok(Box::new(RunkiApp::new(config, initial_deck)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn no_arguments_opens_the_library() {
        assert_eq!(parse(&[]), Ok(Command::Run(None)));
    }

    #[test]
    fn a_path_opens_that_deck() {
        assert_eq!(
            parse(&["deck.apkg"]),
            Ok(Command::Run(Some(PathBuf::from("deck.apkg"))))
        );
    }

    #[test]
    fn version_and_help_flags() {
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
    }

    #[test]
    fn finder_process_serial_number_is_ignored() {
        assert_eq!(parse(&["-psn_0_12345"]), Ok(Command::Run(None)));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        assert!(parse(&["--verbose"]).is_err());
    }
}
