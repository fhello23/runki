use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::{App, Dialog, PathAction};
use crate::library::{self, DeckStatus};
use crate::progress::{self, CardFilter, CardOrder};
use crate::VERSION;

/// Card and file text is untrusted terminal output. Preserve line breaks and
/// tabs, but never send escape sequences or other terminal controls to stdout.
fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let fg = if app.light {
        Color::Black
    } else {
        Color::White
    };
    let bg = if app.light {
        Color::White
    } else {
        Color::Black
    };
    frame.render_widget(Block::default().style(Style::default().fg(fg).bg(bg)), area);
    if area.width < 32 || area.height < 10 {
        frame.render_widget(
            Paragraph::new(if app.dialog.is_some() {
                "Enlarge terminal to at least 32 x 10 to read the dialog. Esc cancel · Ctrl+c quit"
            } else {
                "Enlarge terminal to at least 32 x 10.\nq save and quit | ? help"
            })
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let message = app
        .save_error
        .as_deref()
        .or(app.error.as_deref())
        .or(app.notice.as_deref())
        .map(str::to_owned);
    let message_height = if message.is_some() {
        3.min(area.height / 3)
    } else {
        0
    };
    let [header, status, body, banner, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(message_height),
        Constraint::Length(2),
    ])
    .areas(area);
    let title = app
        .session
        .as_ref()
        .map(|s| s.deck().name.as_str())
        .unwrap_or("Library");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" Runki {VERSION}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  /  {}", safe(title))),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        header,
    );
    if app.session.is_some() {
        study(frame, app, status, body);
    } else {
        library_screen(frame, app, status, body);
    }
    if let Some(message) = message {
        let color = if app.save_error.is_some() || app.error.is_some() {
            Color::Red
        } else {
            Color::Cyan
        };
        frame.render_widget(
            Paragraph::new(format!("[e: full message] {}", safe(&message)))
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: false }),
            banner,
        );
    }
    let hints = if app.session.is_some() {
        " Space reveal/next  l later  1/2/3 filter  d/s order  r shuffle  R reset  b library  o open  ? help  q quit"
    } else {
        " ↑/↓ select  Enter study  a add  c folder  o open  r refresh  t theme  ? help  q quit"
    };
    frame.render_widget(
        Paragraph::new(hints)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::TOP)),
        footer,
    );
    if app.dialog.is_some() {
        draw_dialog(frame, app, area);
    }
}

fn library_screen(frame: &mut Frame, app: &mut App, status: Rect, body: Rect) {
    let folder = app
        .config
        .library
        .as_ref()
        .map(|p| safe(&p.display().to_string()))
        .unwrap_or_else(|| "No library folder chosen — press c to choose or create one".into());
    frame.render_widget(
        Paragraph::new(format!(" {folder}")).wrap(Wrap { trim: false }),
        status,
    );
    if app.entries.is_empty() {
        let text = if app.config.library.is_some() {
            "Your library has no decks.\n\nPress a to copy a deck here, o to open a deck elsewhere, or c to change folder.\n\nSupported: .apkg, .csv, .tsv, .txt"
        } else {
            "Welcome to Runki\n\nPress c to choose a library folder, or o to open a deck.\nYour library and progress work in both the desktop and terminal interfaces.\n\nPress ? for all controls."
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Library ")),
            body,
        );
        return;
    }
    let wide = body.width >= 76;
    let now = progress::now_ms();
    let rows: Vec<Row> = app
        .entries
        .iter()
        .map(|entry| {
            let summary = match &entry.status {
                DeckStatus::Loading => "Loading…".into(),
                DeckStatus::NotDownloaded => "Not downloaded; download in file manager".into(),
                DeckStatus::Failed(err) => format!("Error: {}", safe(err)),
                DeckStatus::Ready { total, seen, later } => {
                    format!("{seen}/{total} seen · {later} later")
                }
            };
            let mut cells = vec![safe(&entry.display_name), summary];
            if wide {
                cells.push(library::last_opened_label(entry.opened_at, now));
            }
            let style = match entry.status {
                DeckStatus::Failed(_) => Style::default().fg(Color::Red),
                DeckStatus::NotDownloaded => Style::default().fg(Color::DarkGray),
                _ => Style::default(),
            };
            Row::new(cells).style(style)
        })
        .collect();
    let (widths, labels) = if wide {
        (
            vec![
                Constraint::Percentage(42),
                Constraint::Min(25),
                Constraint::Length(15),
            ],
            vec!["Deck", "Progress", "Last opened"],
        )
    } else {
        (
            vec![Constraint::Percentage(45), Constraint::Min(15)],
            vec!["Deck", "Progress"],
        )
    };
    // The selected row's full details remain available even if a narrow table
    // truncates its status (including parse failures and placeholder paths).
    let [table_area, details] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(body);
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(Row::new(labels).style(Style::default().fg(Color::Cyan)))
            .row_highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
            .highlight_symbol("› ")
            .block(Block::bordered().title(" Decks ")),
        table_area,
        &mut app.selection,
    );
    if let Some(entry) = app.selection.selected().and_then(|i| app.entries.get(i)) {
        let mut text = safe(&entry.path.display().to_string());
        match &entry.status {
            DeckStatus::Failed(err) => text.push_str(&format!("\n{}", safe(err))),
            DeckStatus::NotDownloaded => {
                text.push_str("\nDownload in your file manager, then press r to refresh.")
            }
            _ => {}
        }
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), details);
    }
}

fn study(frame: &mut Frame, app: &mut App, status: Rect, body: Rect) {
    let session = app.session.as_ref().unwrap();
    let order = match session.card_order() {
        CardOrder::Deck => "Deck order",
        CardOrder::Shuffled => "Shuffled",
    };
    let filter = match session.filter() {
        CardFilter::All => "All",
        CardFilter::Unseen => "Unseen",
        CardFilter::Later => "Study later",
    };
    let saving = if session.save_blocked() {
        " · READ-ONLY PROGRESS"
    } else if session.dirty() {
        " · unsaved"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {order} · {filter} · {}/{} seen · {} later{saving}",
            session.seen_count(),
            session.total(),
            session.later_count()
        ))
        .wrap(Wrap { trim: false }),
        status,
    );
    let Some(card) = session.current_card() else {
        let text = match session.filter() {
            CardFilter::Unseen => "All cards have been seen.\n\nPress R to reset seen progress (keeps study-later flags), or 1 to show all cards.",
            CardFilter::Later => "No study-later cards.\n\nPress 1 to show all cards, then l to flag a card.",
            CardFilter::All => "No cards in this deck.",
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Study ")),
            body,
        );
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(
        "QUESTION",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::default());
    lines.extend(
        safe(&card.front)
            .split('\n')
            .map(|s| Line::from(s.to_owned())),
    );
    lines.push(Line::default());
    if session.revealed() {
        lines.push(Line::styled(
            "ANSWER",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::default());
        lines.extend(
            safe(&card.back)
                .split('\n')
                .map(|s| Line::from(s.to_owned())),
        );
    } else {
        lines.push(Line::styled(
            "Press Space to reveal the answer",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let (n, total) = session.view_position().unwrap_or((0, 0));
    let flag = if session.is_later(&card.id) {
        " · Study later"
    } else {
        ""
    };
    let seen = if session.is_seen(&card.id) {
        " · Seen"
    } else {
        ""
    };
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let content_height = paragraph.line_count(body.width.saturating_sub(2));
    app.max_scroll = content_height
        .saturating_sub(body.height.saturating_sub(2) as usize)
        .min(u16::MAX as usize) as u16;
    app.scroll = app.scroll.min(app.max_scroll);
    let scroll_hint = if app.max_scroll > 0 {
        " · ↑/↓ scroll"
    } else {
        ""
    };
    frame.render_widget(
        paragraph
            .scroll((app.scroll, 0))
            .block(Block::bordered().title(format!(" Card {n}/{total}{flag}{seen}{scroll_hint} "))),
        body,
    );
}

fn draw_dialog(frame: &mut Frame, app: &mut App, area: Rect) {
    let dialog = app.dialog.as_ref().unwrap();
    let (title, text) = match dialog {
        Dialog::Path { action, input } => {
            let title = match action { PathAction::Open => "Open deck", PathAction::Add => "Copy deck into library", PathAction::Library => "Library folder (created if needed)" };
            // Show the tail while editing, leaving enough room for wide glyphs.
            let capacity = area.width.saturating_sub(10) as usize / 2;
            let tail: String = input.chars().rev().take(capacity).collect::<Vec<_>>().into_iter().rev().collect();
            (title, format!("{}Path (absolute, relative, or ~/):\n\n{}▏\n\nEnter accept · Esc cancel · Ctrl+u clear · Backspace delete", app.error.as_ref().map(|e| format!("{}\n\n", safe(e))).unwrap_or_default(), safe(&tail)))
        }
        Dialog::Reset => ("Reset seen progress?", "Clear seen marks for every card in this deck?\nStudy-later flags are kept.\n\ny / Enter reset · n / Esc cancel".into()),
        Dialog::Discard(_) => ("Progress could not be saved", "Your session is still open. Fix the save error and try again, or continue and lose unsaved changes.\n\ny discard changes and continue · n / Esc keep studying".into()),
        Dialog::Message(message) => ("Message · ↑/↓ scroll · Esc close", safe(message)),
        Dialog::Help => ("Controls · ↑/↓ scroll · Esc close", "EVERYWHERE\nq / Ctrl+c save and quit · o open deck · t light/dark\ne full message / error\n? help · Esc dismiss message or cancel dialog\n\nLIBRARY\n↑/↓ or j/k select · Enter study · a copy deck into library\nc choose/create folder · r refresh · Home/End first/last\nPageUp/PageDown move ten rows\n\nSTUDY\nSpace / Enter reveal answer, then next card (wraps)\nl toggle study later · b save and return to library\n1 all · 2 unseen · 3 study later\nd deck order · s shuffled · r reshuffle\nR reset seen progress (asks first; flags kept)\n↑/↓ or j/k scroll · PageUp/PageDown scroll ten lines\nHome/End top/bottom of card\n\nPATH INPUT: type or paste · Backspace delete · Ctrl+u clear\n\nEnter / ? / Esc close help".into()),
    };
    let width = area.width.saturating_sub(4).min(90);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let lines = paragraph
        .line_count(width.saturating_sub(2))
        .min(u16::MAX as usize) as u16;
    let height = lines.saturating_add(2).min(area.height.saturating_sub(2));
    app.dialog_max_scroll = lines.saturating_sub(height.saturating_sub(2));
    app.dialog_scroll = app.dialog_scroll.min(app.dialog_max_scroll);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        paragraph.scroll((app.dialog_scroll, 0)).block(
            Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn removes_terminal_controls_from_untrusted_content() {
        assert_eq!(
            super::safe("hello\x1b[2J\r\x07\u{009b}world\n\tend"),
            "hello[2Jworld\n\tend"
        );
    }
}
