use super::*;
use crate::library::DeckStatus;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn setup() -> (App, tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let library = std::fs::canonicalize(dir.path()).unwrap().join("decks");
    std::fs::create_dir(&library).unwrap();
    let deck = library.join("test.tsv");
    std::fs::write(
        &deck,
        "front\tback\nquestion one\tanswer one\nquestion two\tanswer two\n",
    )
    .unwrap();
    let config = Config {
        library: Some(library),
        last_open_deck: None,
        ..Default::default()
    };
    let app = App::new(config, dir.path().join("config.json"));
    (app, dir, deck)
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}
fn char_key(app: &mut App, c: char) {
    key(app, KeyCode::Char(c));
}
fn session(app: &App) -> &StudySession {
    app.session.as_ref().unwrap()
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn studying_saves_and_resumes_in_the_shared_library_state() {
    let (mut app, _dir, deck) = setup();
    app.open(deck.clone()).unwrap();
    let first = session(&app).current_id().unwrap().to_string();
    char_key(&mut app, ' ');
    char_key(&mut app, 'l');
    assert!(session(&app).revealed());
    assert_eq!(session(&app).seen_count(), 1);
    char_key(&mut app, 's');
    char_key(&mut app, 'r');
    assert_eq!(session(&app).current_id(), Some(first.as_str()));
    char_key(&mut app, '3');
    assert_eq!(session(&app).filter(), CardFilter::Later);
    char_key(&mut app, 'b');
    assert!(app.session.is_none());
    let state_path = progress::state_path(&deck, app.config.library.as_deref());
    assert!(state_path.starts_with(app.config.library.as_ref().unwrap().join(".runki")));
    let state = progress::load(&deck, &state_path).into_state();
    assert!(state.is_seen(&first));
    assert!(state.is_later(&first));
    assert_eq!(state.prefs.order, CardOrder::Shuffled);
    assert_eq!(state.prefs.filter, CardFilter::Later);
    app.open(deck).unwrap();
    assert_eq!(session(&app).current_id(), Some(first.as_str()));
    assert!(!session(&app).revealed());
}

#[test]
fn unseen_filter_retains_revealed_card_then_becomes_empty_and_reset_keeps_flags() {
    let (mut app, _dir, deck) = setup();
    app.open(deck).unwrap();
    char_key(&mut app, 'l');
    char_key(&mut app, '2');
    for _ in 0..2 {
        char_key(&mut app, ' ');
        assert!(session(&app).current_card().is_some());
        char_key(&mut app, ' ');
    }
    assert!(session(&app).current_card().is_none());
    assert!(render(&mut app, 90, 24).contains("All cards have been seen"));
    char_key(&mut app, 'R');
    key(&mut app, KeyCode::Esc);
    assert_eq!(session(&app).seen_count(), 2);
    char_key(&mut app, 'R');
    char_key(&mut app, 'y');
    assert_eq!(session(&app).seen_count(), 0);
    assert_eq!(session(&app).later_count(), 1);
    assert!(session(&app).current_card().is_some());
}

#[test]
fn unflagging_the_last_later_card_updates_the_empty_view() {
    let (mut app, _dir, deck) = setup();
    app.open(deck).unwrap();
    char_key(&mut app, 'l');
    char_key(&mut app, '3');
    char_key(&mut app, 'l');
    assert!(session(&app).current_card().is_none());
    assert!(render(&mut app, 80, 24).contains("No study-later cards"));
}

#[test]
fn failed_save_retains_session_and_requires_explicit_discard() {
    let (mut app, _dir, deck) = setup();
    app.open(deck.clone()).unwrap();
    char_key(&mut app, 'l');
    let state_path = session(&app).state_path().clone();
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, "broken").unwrap();
    char_key(&mut app, 'b');
    assert!(app.session.is_some());
    assert!(session(&app).dirty());
    assert!(matches!(
        app.dialog,
        Some(Dialog::Discard(Navigation::Library))
    ));
    key(&mut app, KeyCode::Esc);
    char_key(&mut app, 'q');
    assert!(!app.quit);
    char_key(&mut app, 'n');
    std::fs::remove_file(&state_path).unwrap();
    char_key(&mut app, 'q');
    assert!(app.quit);
    assert_eq!(
        progress::load(&deck, &state_path)
            .state()
            .later_count(session(&app).deck().cards.iter().map(|c| c.id.as_str())),
        1
    );
}

#[test]
fn corrupt_progress_is_preserved_and_deck_can_be_reopened_after_discard() {
    let (mut app, _dir, deck) = setup();
    let state_path = progress::state_path(&deck, app.config.library.as_deref());
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, "broken").unwrap();
    app.open(deck.clone()).unwrap();
    assert!(session(&app).save_blocked());
    char_key(&mut app, ' ');
    char_key(&mut app, 'q');
    assert!(!app.quit);
    assert_eq!(std::fs::read_to_string(&state_path).unwrap(), "broken");
    key(&mut app, KeyCode::Esc);
    app.open(deck.clone()).unwrap_err();
    assert!(matches!(
        app.dialog,
        Some(Dialog::Discard(Navigation::Open(_)))
    ));
    std::fs::remove_file(&state_path).unwrap();
    char_key(&mut app, 'y');
    assert!(!session(&app).save_blocked());
}

#[test]
fn failed_open_keeps_current_deck_and_prompt_edits_do_not_trigger_study_keys() {
    let (mut app, _dir, deck) = setup();
    app.open(deck.clone()).unwrap();
    assert!(app.open(deck.with_file_name("missing.tsv")).is_err());
    assert_eq!(session(&app).deck().path, deck);
    char_key(&mut app, 'o');
    for c in "l s q".chars() {
        char_key(&mut app, c);
    }
    assert_eq!(session(&app).later_count(), 0);
    assert!(!session(&app).revealed());
    assert!(!app.quit);
    key(&mut app, KeyCode::Enter);
    assert!(matches!(app.dialog, Some(Dialog::Path { .. })));
    assert!(app.error.is_some());
    key(&mut app, KeyCode::Esc);
    assert_eq!(session(&app).deck().path, deck);
}

#[test]
fn paths_support_unicode_paste_clear_and_home() {
    let (mut app, _dir, _) = setup();
    char_key(&mut app, 'o');
    char_key(&mut app, 'é');
    key(&mut app, KeyCode::Backspace);
    assert!(matches!(&app.dialog, Some(Dialog::Path { input, .. }) if input.is_empty()));
    char_key(&mut app, 'x');
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(matches!(&app.dialog, Some(Dialog::Path { input, .. }) if input.is_empty()));
    app.handle_event(Event::Paste("my decks/é.tsv\r\n".into()));
    assert!(matches!(&app.dialog, Some(Dialog::Path { input, .. }) if input == "my decks/é.tsv"));
    assert_eq!(
        input_path("  'a path/é.tsv' ").unwrap(),
        PathBuf::from("a path/é.tsv")
    );
    assert_eq!(
        input_path("~/decks").unwrap(),
        dirs::home_dir().unwrap().join("decks")
    );
    assert!(input_path(" ").is_err());
}

#[test]
fn add_does_not_overwrite_decks_and_remembers_changed_folder() {
    let (mut app, dir, deck) = setup();
    let source = dir.path().join("source.tsv");
    std::fs::copy(&deck, &source).unwrap();
    let new_dir = dir.path().join("new library");
    app.submit_path(PathAction::Library, new_dir.to_str().unwrap())
        .unwrap();
    app.submit_path(PathAction::Add, source.to_str().unwrap())
        .unwrap();
    app.submit_path(PathAction::Add, source.to_str().unwrap())
        .unwrap();
    assert!(new_dir.join("source.tsv").exists());
    assert!(new_dir.join("source (2).tsv").exists());
    assert_eq!(
        config::load(&app.config_path).unwrap().unwrap().library,
        Some(std::fs::canonicalize(new_dir).unwrap())
    );
}

#[test]
fn long_unicode_cards_scroll_to_the_end_and_controls_are_sanitized() {
    let (mut app, _dir, deck) = setup();
    let front = (0..80)
        .map(|i| format!("Question {i} 日本語 🦀"))
        .collect::<Vec<_>>()
        .join("\n");
    let back = format!("{}\nANSWER-END", "answer ".repeat(300));
    std::fs::write(&deck, format!("\"{front}\"\t\"{back}\"\n")).unwrap();
    app.open(deck).unwrap();
    assert!(render(&mut app, 60, 20).contains("Question 0"));
    char_key(&mut app, ' ');
    render(&mut app, 60, 20);
    assert!(app.max_scroll > 80);
    key(&mut app, KeyCode::End);
    assert!(render(&mut app, 60, 20).contains("ANSWER-END"));
    char_key(&mut app, ' ');
    assert_eq!(app.scroll, 0);
}

#[test]
fn library_background_updates_keep_selection_by_path_and_list_failures() {
    let (mut app, _dir, deck) = setup();
    let other = deck.with_file_name("z.tsv");
    std::fs::write(&other, "q\ta\n").unwrap();
    std::fs::write(deck.with_file_name("bad.apkg"), "not a zip").unwrap();
    std::fs::write(deck.with_file_name(".cloud.apkg.icloud"), "").unwrap();
    app.rescan();
    app.restore_selection(Some(&other));
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.entries.iter().any(|e| e.status == DeckStatus::Loading) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.tick();
    }
    assert!(app
        .entries
        .iter()
        .any(|e| matches!(e.status, DeckStatus::Failed(_))));
    assert!(app
        .entries
        .iter()
        .any(|e| e.status == DeckStatus::NotDownloaded));
    assert_eq!(app.selected_path(), Some(other));
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("Error:"));
    assert!(screen.contains("Not downloaded"));
}

#[test]
fn all_layouts_handle_resizing_and_help_can_scroll_in_small_terminals() {
    let (mut app, _dir, deck) = setup();
    for (width, height) in [(100, 30), (80, 24), (40, 12), (32, 10), (10, 4), (0, 0)] {
        render(&mut app, width, height);
    }
    app.open(deck).unwrap();
    for (width, height) in [(100, 30), (40, 12), (32, 10), (10, 4)] {
        render(&mut app, width, height);
    }
    char_key(&mut app, '?');
    render(&mut app, 40, 12);
    assert!(app.dialog_max_scroll > 0);
    key(&mut app, KeyCode::End);
    assert!(render(&mut app, 40, 12).contains("close help"));
}

#[test]
fn key_release_and_repeat_do_not_advance_or_reveal() {
    let (mut app, _dir, deck) = setup();
    app.open(deck).unwrap();
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        let mut event = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        event.kind = kind;
        app.handle_key(event);
    }
    assert!(!session(&app).revealed());
    assert_eq!(session(&app).seen_count(), 0);
}

#[cfg(unix)]
#[test]
fn deck_symlinks_use_the_same_progress_path_as_the_desktop() {
    use std::os::unix::fs::symlink;
    let (mut app, dir, deck) = setup();
    let outside = dir.path().join("outside.tsv");
    std::fs::rename(&deck, &outside).unwrap();
    symlink(&outside, &deck).unwrap();
    app.open(deck.clone()).unwrap();
    assert_eq!(
        session(&app).state_path(),
        &progress::state_path(&deck, app.config.library.as_deref())
    );
    char_key(&mut app, 'l');
    app.save().unwrap();
    assert!(!progress::state_path(&outside, None).exists());
    assert!(session(&app).state_path().exists());
}

#[test]
fn terminal_theme_choice_is_remembered() {
    let (mut app, _dir, _) = setup();
    char_key(&mut app, 't');
    let config = config::load(&app.config_path).unwrap().unwrap();
    assert!(config.terminal_light);
    let restored = App::new(config, app.config_path.clone());
    assert!(restored.light);
}

#[test]
fn terminal_entry_point_rejects_redirected_io_without_creating_settings() {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let err = run(Config::default(), path.clone(), None).unwrap_err();
    assert!(err.to_string().contains("interactive terminal"));
    assert!(!path.exists());
}
