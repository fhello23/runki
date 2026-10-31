//! Terminal front-end, using the same study sessions and library loader as egui.

mod ui;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::DefaultTerminal;

use crate::config::{self, Config};
use crate::deck;
use crate::library::{self, DeckEntry, LibraryLoader};
use crate::progress::{self, CardFilter, CardOrder};
use crate::session::StudySession;

const SAVE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathAction {
    Open,
    Add,
    Library,
}

#[derive(Debug)]
enum Navigation {
    Quit,
    Library,
    Open(PathBuf),
    SetLibrary(PathBuf),
}

#[derive(Debug)]
enum Dialog {
    Path { action: PathAction, input: String },
    Reset,
    Discard(Navigation),
    Message(String),
    Help,
}

struct App {
    config: Config,
    config_path: PathBuf,
    entries: Vec<DeckEntry>,
    selection: ratatui::widgets::TableState,
    loader: LibraryLoader,
    session: Option<StudySession>,
    dialog: Option<Dialog>,
    dialog_scroll: u16,
    dialog_max_scroll: u16,
    error: Option<String>,
    save_error: Option<String>,
    notice: Option<String>,
    last_save: Instant,
    scroll: u16,
    max_scroll: u16,
    light: bool,
    quit: bool,
}

impl App {
    fn new(config: Config, config_path: PathBuf) -> Self {
        let light = config.terminal_light;
        let mut app = Self {
            config,
            config_path,
            entries: Vec::new(),
            selection: ratatui::widgets::TableState::default(),
            loader: LibraryLoader::new(Box::new(|| {})),
            session: None,
            dialog: None,
            dialog_scroll: 0,
            dialog_max_scroll: 0,
            error: None,
            save_error: None,
            notice: None,
            last_save: Instant::now(),
            scroll: 0,
            max_scroll: 0,
            light,
            quit: false,
        };
        app.rescan();
        app
    }

    fn persist_config(&mut self) {
        if let Err(err) = config::save(&self.config_path, &self.config) {
            self.error = Some(format!("Settings could not be saved: {err:#}"));
        }
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selection
            .selected()
            .and_then(|i| self.entries.get(i))
            .map(|entry| entry.path.clone())
    }

    fn restore_selection(&mut self, path: Option<&Path>) {
        let selected = path
            .and_then(|path| self.entries.iter().position(|e| e.path == path))
            .or_else(|| (!self.entries.is_empty()).then_some(0));
        self.selection.select(selected);
    }

    fn rescan(&mut self) {
        let selected = self.selected_path();
        self.entries.clear();
        if let Some(dir) = &self.config.library {
            match library::scan(dir) {
                Ok(entries) => self.entries = entries,
                Err(err) => self.error = Some(format!("{err:#}")),
            }
            // Also supersedes any old scan when this one failed.
            self.loader
                .start(dir, library::loadable_paths(&self.entries));
        }
        self.restore_selection(selected.as_deref());
    }

    fn tick(&mut self) {
        let selected = self.selected_path();
        let summaries = self.loader.drain();
        if !summaries.is_empty() {
            for summary in summaries {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.path == summary.path) {
                    entry.status = summary.status;
                    entry.opened_at = summary.opened_at;
                }
            }
            library::sort_entries(&mut self.entries);
            self.restore_selection(selected.as_deref());
        }
        if self.last_save.elapsed() >= SAVE_INTERVAL {
            let _ = self.save();
        }
    }

    fn save(&mut self) -> Result<()> {
        self.last_save = Instant::now();
        if let Some(session) = &mut self.session {
            if session.save_blocked() && session.dirty() {
                let message = "Progress could not be read when this deck opened. This session's changes cannot be saved; fix the progress file and reopen the deck.";
                self.save_error = Some(message.into());
                bail!("{message}");
            }
            if let Err(err) = session.save() {
                self.save_error = Some(format!("Progress could not be saved: {err:#}"));
                return Err(err);
            }
        }
        self.save_error = None;
        Ok(())
    }

    fn open(&mut self, path: PathBuf) -> Result<()> {
        // Make paths independent of the working directory without resolving a
        // deck symlink: its filename determines the shared GUI progress path.
        if library::is_dataless(&path)
            || (!path.exists() && library::icloud_placeholder_path(&path).exists())
        {
            bail!(
                "{} is not downloaded. Download it in your file manager, then refresh.",
                path.display()
            );
        }
        let path = std::path::absolute(&path)
            .with_context(|| format!("Could not open {}", path.display()))?;
        let deck =
            deck::load_deck(&path).with_context(|| format!("Could not open {}", path.display()))?;
        self.save_before_leaving(Navigation::Open(path.clone()))?;
        let state_path = progress::state_path(&path, self.config.library.as_deref());
        let outcome = progress::load(&path, &state_path);
        let blocked = outcome.is_blocked();
        self.error = outcome.error().map(|err| format!("{err:#}. Progress is read-only for this session; fix or remove that file and reopen the deck."));
        self.config.last_open_deck = Some(path);
        self.session = Some(StudySession::new(
            deck,
            outcome.into_state(),
            state_path,
            blocked,
            progress::now_ms(),
        ));
        self.scroll = 0;
        self.max_scroll = 0;
        self.persist_config();
        Ok(())
    }

    fn go_to_library(&mut self) -> Result<()> {
        self.save_before_leaving(Navigation::Library)?;
        self.session = None;
        self.rescan();
        Ok(())
    }

    fn save_before_leaving(&mut self, next: Navigation) -> Result<()> {
        if let Err(err) = self.save() {
            self.dialog_scroll = 0;
            self.dialog = Some(Dialog::Discard(next));
            return Err(err);
        }
        Ok(())
    }

    fn request_quit(&mut self) {
        match self.save() {
            Ok(()) => self.quit = true,
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.dialog_scroll = 0;
                self.dialog = Some(Dialog::Discard(Navigation::Quit));
            }
        }
    }

    fn prompt(&mut self, action: PathAction) {
        let input = if action == PathAction::Library {
            self.config
                .library
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.dialog_scroll = 0;
        self.dialog = Some(Dialog::Path { action, input });
    }

    fn submit_path(&mut self, action: PathAction, input: &str) -> Result<()> {
        let path = input_path(input)?;
        match action {
            PathAction::Open => self.open(path)?,
            PathAction::Add => {
                let dir = self
                    .config
                    .library
                    .as_ref()
                    .context("Choose a library folder with c first")?;
                let target = library::add_deck(dir, &path)?;
                self.notice = Some(format!("Added {}", target.display()));
                self.rescan();
                self.restore_selection(Some(&target));
            }
            PathAction::Library => {
                self.save_before_leaving(Navigation::SetLibrary(path.clone()))?;
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("Could not create {}", path.display()))?;
                self.config.library = Some(std::fs::canonicalize(path)?);
                self.session = None;
                self.persist_config();
                self.rescan();
            }
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let index = self.selection.selected().unwrap_or(0);
        let index = index
            .saturating_add_signed(delta)
            .min(self.entries.len() - 1);
        self.selection.select(Some(index));
    }

    fn handle_dialog(&mut self, key: KeyEvent) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        if key.code == KeyCode::Esc {
            return;
        }
        let scroll = match key.code {
            KeyCode::Down => Some(
                self.dialog_scroll
                    .saturating_add(1)
                    .min(self.dialog_max_scroll),
            ),
            KeyCode::Up => Some(self.dialog_scroll.saturating_sub(1)),
            KeyCode::PageDown => Some(
                self.dialog_scroll
                    .saturating_add(10)
                    .min(self.dialog_max_scroll),
            ),
            KeyCode::PageUp => Some(self.dialog_scroll.saturating_sub(10)),
            _ => None,
        };
        if let Some(scroll) = scroll {
            self.dialog_scroll = scroll;
            self.dialog = Some(dialog);
            return;
        }
        if matches!(dialog, Dialog::Help | Dialog::Message(_)) {
            match key.code {
                KeyCode::Char('j') => {
                    self.dialog_scroll = self
                        .dialog_scroll
                        .saturating_add(1)
                        .min(self.dialog_max_scroll)
                }
                KeyCode::Char('k') => self.dialog_scroll = self.dialog_scroll.saturating_sub(1),
                KeyCode::Home => self.dialog_scroll = 0,
                KeyCode::End => self.dialog_scroll = self.dialog_max_scroll,
                KeyCode::Char('?' | 'q') | KeyCode::Enter => return,
                _ => {}
            }
            self.dialog = Some(dialog);
            return;
        }
        match dialog {
            Dialog::Path { action, mut input } => {
                match key.code {
                    KeyCode::Enter => {
                        self.error = None;
                        if let Err(err) = self.submit_path(action, &input) {
                            self.error = Some(format!("{err:#}"));
                            if self.dialog.is_none() {
                                self.dialog = Some(Dialog::Path { action, input });
                            }
                        }
                        return;
                    }
                    KeyCode::Backspace => {
                        self.dialog_scroll = 0;
                        input.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.dialog_scroll = 0;
                        input.clear()
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.dialog_scroll = 0;
                        input.push(c)
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Path { action, input });
            }
            Dialog::Reset => {
                if matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
                    if let Some(session) = &mut self.session {
                        session.reset(progress::now_ms());
                    }
                    self.scroll = 0;
                } else if !matches!(key.code, KeyCode::Char('n' | 'N')) {
                    self.dialog = Some(Dialog::Reset);
                }
            }
            Dialog::Discard(next) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.session = None;
                    self.error = None;
                    self.save_error = None;
                    let result = match next {
                        Navigation::Quit => {
                            self.quit = true;
                            Ok(())
                        }
                        Navigation::Library => {
                            self.rescan();
                            Ok(())
                        }
                        Navigation::Open(path) => self.open(path),
                        Navigation::SetLibrary(path) => {
                            self.submit_path(PathAction::Library, &path.to_string_lossy())
                        }
                    };
                    if let Err(err) = result {
                        self.error = Some(format!("{err:#}"));
                    }
                } else if !matches!(key.code, KeyCode::Char('n' | 'N')) {
                    self.dialog = Some(Dialog::Discard(next));
                }
            }
            Dialog::Help | Dialog::Message(_) => unreachable!(),
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                if let Some(Dialog::Path { input, .. }) = &mut self.dialog {
                    self.dialog_scroll = 0;
                    input.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ignore key-up and explicit autorepeat events.
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if self.dialog.is_some() {
            self.handle_dialog(key);
            return;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return;
        }
        if key.code != KeyCode::Char('e') {
            self.notice = None;
        }
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => {
                self.dialog_scroll = 0;
                self.dialog = Some(Dialog::Help);
            }
            KeyCode::Char('e') => {
                if let Some(message) = self
                    .save_error
                    .as_ref()
                    .or(self.error.as_ref())
                    .or(self.notice.as_ref())
                {
                    self.dialog_scroll = 0;
                    self.dialog = Some(Dialog::Message(message.clone()));
                }
            }
            KeyCode::Char('t') => {
                self.light = !self.light;
                self.config.terminal_light = self.light;
                self.persist_config();
            }
            KeyCode::Char('o') => self.prompt(PathAction::Open),
            KeyCode::Char('a') if self.session.is_none() => self.prompt(PathAction::Add),
            KeyCode::Char('c') if self.session.is_none() => self.prompt(PathAction::Library),
            KeyCode::Esc => {
                self.error = None;
            }
            KeyCode::Char('b') if self.session.is_some() => {
                if let Err(err) = self.go_to_library() {
                    self.error = Some(format!("{err:#}"));
                }
            }
            _ if self.session.is_none() => self.library_key(key.code),
            _ => self.study_key(key.code),
        }
    }

    fn library_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self
                .selection
                .select((!self.entries.is_empty()).then_some(0)),
            KeyCode::End => self.selection.select(self.entries.len().checked_sub(1)),
            KeyCode::Char('r') => {
                self.error = None;
                self.rescan();
            }
            KeyCode::Enter => {
                if let Some(path) = self.selected_path() {
                    if let Err(err) = self.open(path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
            }
            _ => {}
        }
    }

    fn study_key(&mut self, code: KeyCode) {
        let Some(session) = &mut self.session else {
            return;
        };
        let now = progress::now_ms();
        let mut reset_scroll = true;
        match code {
            KeyCode::Char(' ') | KeyCode::Enter => session.space(now),
            KeyCode::Char('l') => session.toggle_later(now),
            KeyCode::Char('d') => session.set_order(CardOrder::Deck, now),
            KeyCode::Char('s') => session.set_order(CardOrder::Shuffled, now),
            KeyCode::Char('r') if session.card_order() == CardOrder::Shuffled => {
                session.reshuffle(now)
            }
            KeyCode::Char('1') => session.set_filter(CardFilter::All, now),
            KeyCode::Char('2') => session.set_filter(CardFilter::Unseen, now),
            KeyCode::Char('3') => session.set_filter(CardFilter::Later, now),
            KeyCode::Char('R') => {
                self.dialog_scroll = 0;
                self.dialog = Some(Dialog::Reset);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll);
                reset_scroll = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                reset_scroll = false;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10).min(self.max_scroll);
                reset_scroll = false;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                reset_scroll = false;
            }
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => {
                self.scroll = self.max_scroll;
                reset_scroll = false;
            }
            _ => reset_scroll = false,
        }
        if reset_scroll {
            self.scroll = 0;
        }
    }
}

fn input_path(input: &str) -> Result<PathBuf> {
    let input = input.trim();
    let input = input
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| input.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(input);
    if input.is_empty() {
        bail!("Enter a path first");
    }
    if input == "~" {
        return dirs::home_dir().context("Could not find the home folder");
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Ok(dirs::home_dir()
            .context("Could not find the home folder")?
            .join(rest));
    }
    Ok(PathBuf::from(input))
}

// try_init installs Ratatui's panic hook. The guard also restores on every
// ordinary error/return, including partially successful terminal setup.
struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), event::DisableBracketedPaste);
        ratatui::restore();
    }
}

pub fn run(config: Config, config_path: PathBuf, initial_deck: Option<PathBuf>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The terminal interface needs an interactive terminal on stdin and stdout. Run runki --tui in a terminal without piping or redirection.");
    }
    let mut app = App::new(config, config_path);
    if let Some(path) = initial_deck {
        app.open(path)?;
    }
    app.persist_config();
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init().context("Could not initialize the terminal")?;
    execute!(io::stdout(), event::EnableBracketedPaste)?;
    let result = event_loop(&mut terminal, &mut app);
    // Even a failed read or draw gets a final save attempt.
    if !app.quit {
        app.save()
            .context("Could not save progress while closing the terminal interface")?;
    }
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        app.handle_event(event::read()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
