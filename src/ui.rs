use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, LineGauge, Paragraph, Row, Table, Wrap};
use ratatui_image::{FilterType, Resize, StatefulImage};

use crate::app::{App, Pane, Tab};
use crate::library::fmt_duration;

const ACCENT: Color = Color::Rgb(137, 180, 250);
const ACCENT_ALT: Color = Color::Rgb(203, 166, 247);
const DIM: Color = Color::Rgb(88, 91, 112);
const TEXT: Color = Color::Rgb(205, 214, 244);

/// Marks the recursive track count in the folder pane. A BMP glyph on purpose:
/// emoji are double-width and would shift the column.
const COUNT_MARK: &str = "♪";

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let border = if focused { ACCENT } else { DIM };
    let title_style = if focused {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(DIM)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Span::styled(format!(" {title} "), title_style))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, help] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let [left, middle, right] = Layout::horizontal([
        Constraint::Percentage(24),
        Constraint::Percentage(44),
        Constraint::Percentage(32),
    ])
    .areas(main);

    // Always drawn: switching sources must not look like playback stopped.
    draw_now_playing(frame, app, right);

    match app.tab {
        Tab::Local => {
            draw_folders(frame, app, left);
            draw_tracks(frame, app, middle);
        }
        Tab::Radio => {
            // Forget where the rows were, or clicks would still land on panes that
            // are no longer on screen.
            app.folder_rows = Rect::ZERO;
            app.track_rows = Rect::ZERO;
            let span = Rect {
                width: left.width + middle.width,
                ..left
            };
            draw_placeholder(frame, app, span);
        }
    }
    draw_help(frame, app, help);
}

/// Tabs live in the top border of the browsing pane, and their rects are recorded so
/// a click can hit them. Drawn over the border rather than as a `Block` title,
/// because a title's position is not something the caller gets told.
fn draw_tabs(frame: &mut Frame, app: &mut App, pane: Rect) {
    // One cell for the rounded corner; each label carries its own padding.
    let mut x = pane.x.saturating_add(1);
    let limit = pane.right().saturating_sub(1);

    for (index, tab) in Tab::ALL.iter().enumerate() {
        let active = app.tab == *tab;
        let text = format!(" {} ", tab.label());
        let width = text.chars().count() as u16;
        if x + width > limit {
            app.tab_areas[index] = Rect::ZERO;
            continue;
        }
        let area = Rect::new(x, pane.y, width, 1);
        let style = if active {
            Style::new()
                .fg(Color::Rgb(30, 30, 46))
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(DIM)
        };
        frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
        app.tab_areas[index] = area;
        x += width;

        if index + 1 < Tab::ALL.len() && x < limit {
            frame.render_widget(
                Paragraph::new(Span::styled("│", Style::new().fg(DIM))),
                Rect::new(x, pane.y, 1, 1),
            );
            x += 1;
        }
    }
}

/// A source that exists as a tab but not yet as code.
fn draw_placeholder(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(""); (inner.height / 2).saturating_sub(2) as usize];
    lines.push(Line::from(Span::styled(
        "Radio",
        Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "not built yet",
        Style::new().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "see PLAN.md",
        Style::new().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
    draw_tabs(frame, app, area);
}

fn draw_folders(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Folders;
    // The `..` row is part of the table, not a decoration, so a click on it hits the
    // same row arithmetic as any folder. `App::folder_at` owns the offset.
    let mut rows: Vec<Row> = Vec::with_capacity(app.folder_row_count());
    if app.shows_up_row() {
        rows.push(Row::new(vec![
            Cell::from("..").style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Cell::from(""),
        ]));
    }
    rows.extend(app.folders.iter().map(|f| {
        Row::new(vec![
            Cell::from(format!("{}/", f.label)).style(Style::new().fg(TEXT)),
            // Right-aligned so the note sits in a column whatever the digit count.
            Cell::from(Line::from(format!("{} {COUNT_MARK}", f.count)).alignment(Alignment::Right))
                .style(Style::new().fg(DIM)),
        ])
    }));

    // The top border carries the tabs, so the breadcrumb goes along the bottom —
    // where a long path has the full width and cannot collide with them.
    let here = if app.can_leave() {
        format!(" ⌫ {} ", app.here())
    } else {
        format!(" {} ", app.here())
    };
    let block = pane_block("", focused).title_bottom(Span::styled(
        here,
        Style::new().fg(if focused { ACCENT } else { DIM }),
    ));
    let table = Table::new(rows, [Constraint::Min(0), Constraint::Length(7)])
        .header(
            Row::new(vec![
                Cell::from("FOLDER"),
                Cell::from(Line::from("TRACKS").alignment(Alignment::Right)),
            ])
            .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
        )
        .block(block)
        .row_highlight_style(
            Style::new()
                .bg(ACCENT)
                .fg(Color::Rgb(30, 30, 46))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");

    app.folder_rows = rows_area(area);
    frame.render_stateful_widget(table, area, &mut app.folder_state);
    draw_tabs(frame, app, area);
}

/// The data rows of a bordered table with a one-line header — what a click on a
/// row has to be tested against.
fn rows_area(pane: Rect) -> Rect {
    let inner = pane.inner(Margin::new(1, 1));
    Rect {
        y: inner.y.saturating_add(1),
        height: inner.height.saturating_sub(1),
        ..inner
    }
}

fn draw_tracks(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Tracks;
    let playing = app.playing_row();
    let title = if app.tracks_loading {
        "Tracks — scanning…".to_string()
    } else {
        format!("Tracks ({})", app.tracks.len())
    };

    let rows: Vec<Row> = app
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let marker = if Some(i) == playing { "▶" } else { " " };
            let title_style = if Some(i) == playing {
                Style::new().fg(ACCENT_ALT).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(TEXT)
            };
            Row::new(vec![
                Cell::from(marker).style(Style::new().fg(ACCENT_ALT)),
                Cell::from(format!("{:>2}", i + 1)).style(Style::new().fg(DIM)),
                Cell::from(t.title.clone()).style(title_style),
                Cell::from(t.artist.clone()).style(Style::new().fg(DIM)),
                Cell::from(
                    t.duration
                        .map(fmt_duration)
                        .unwrap_or_else(|| "--:--".into()),
                )
                .style(Style::new().fg(DIM)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Percentage(30),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(vec!["", "#", "TITLE", "ARTIST", "TIME"])
            .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
    )
    .block(pane_block(&title, focused))
    .row_highlight_style(
        Style::new()
            .bg(Color::Rgb(49, 50, 68))
            .add_modifier(Modifier::BOLD),
    );

    app.track_rows = rows_area(area);
    frame.render_stateful_widget(table, area, &mut app.track_state);
}

fn draw_now_playing(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = pane_block("Now Playing", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The art gets whatever the info block, progress bar and button do not need.
    const INFO_H: u16 = 3;
    const CONTROLS_H: u16 = 5;
    let budget = inner.height.saturating_sub(INFO_H + CONTROLS_H);
    // The loader needs this to know how far to shrink an oversized cover.
    app.art_budget = Rect::new(inner.x, inner.y, inner.width, budget);
    let art_size = art_cells(app, inner.width, budget);

    // Two flexible gaps centre the art + info group in the space above the
    // controls, instead of stranding it at the top of a tall pane.
    let [_, art, info, _, controls] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(art_size.1),
        Constraint::Length(INFO_H),
        Constraint::Min(0),
        Constraint::Length(CONTROLS_H),
    ])
    .areas(inner);

    draw_art(frame, app, centre_in(art, art_size));
    draw_info(frame, app, info);
    draw_controls(frame, app, controls);
}

/// The render thread never enlarges a cover — `cover.rs` already scaled it to fill
/// the pane on a worker thread.
///
/// Doing it here instead is what made switching tracks feel slow. Cost of the first
/// frame after a new cover, 300 px source, debug build:
///
/// | drawn at      | filter   | first frame |
/// |---------------|----------|-------------|
/// | 300px, as-is  | —        | 16 ms       |
/// | 600px (2x)    | Nearest  | 153 ms      |
/// | 600px (2x)    | Lanczos3 | 450 ms      |
///
/// Four times the pixels to resize, base64 and push through the terminal. The
/// worker pays that off-screen instead, so this stays at 1.0.
const MAX_UPSCALE: f32 = 1.0;

/// Only reached when the pane shrank and the worker has not caught up yet, so a
/// cheap filter is right: Lanczos3 costs 3x more for a frame or two of transition.
const ART_FILTER: FilterType = FilterType::Triangle;

fn cell_size(app: &App) -> (f32, f32) {
    let font = app.picker.font_size();
    (font.width.max(1) as f32, font.height.max(1) as f32)
}

/// Pixel size the cover will actually be drawn at inside a box of `box_px`,
/// preserving its aspect ratio and honouring [`MAX_UPSCALE`].
fn drawn_px(app: &App, box_px: (f32, f32)) -> (f32, f32) {
    // A square placeholder stands in when there is no cover to measure.
    let (img_w, img_h) = app
        .cover_size
        .map(|(w, h)| (w as f32, h as f32))
        .unwrap_or((1.0, 1.0));
    let scale = (box_px.0 / img_w).min(box_px.1 / img_h).min(MAX_UPSCALE);
    (img_w * scale, img_h * scale)
}

/// Size in cells the cover will be drawn at inside a `cols` x `rows` box.
///
/// This is the single source of truth for the art geometry: the layout reserves
/// exactly these rows and centres exactly these columns, so no rounding gap can
/// open up between what is reserved and what is drawn.
fn art_cells(app: &App, cols: u16, rows: u16) -> (u16, u16) {
    if cols == 0 || rows == 0 {
        return (0, 0);
    }
    let (cell_w, cell_h) = cell_size(app);
    let (px_w, px_h) = drawn_px(app, (cols as f32 * cell_w, rows as f32 * cell_h));
    (
        ((px_w / cell_w).round() as u16).clamp(1, cols),
        ((px_h / cell_h).round() as u16).clamp(1, rows),
    )
}

/// Centre a `size` box inside `area`. Split out so it can be reasoned about on
/// its own — `StatefulImage` letterboxes towards the top-left and will not do it.
fn centre_in(area: Rect, (width, height): (u16, u16)) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn draw_art(frame: &mut Frame, app: &mut App, target: Rect) {
    if target.height == 0 || target.width == 0 {
        return;
    }

    if let Some(protocol) = app.cover.as_mut() {
        frame.render_stateful_widget(
            // `Fit` shrinks an oversized cover and leaves everything else alone.
            // `target` is already the exact size the cover will occupy, so there is
            // nothing left to letterbox — and usually nothing to resize either.
            StatefulImage::default().resize(Resize::Fit(Some(ART_FILTER))),
            target,
            protocol,
        );
    } else {
        let caption = if app.cover_pending {
            "loading…"
        } else {
            "no cover"
        };
        let mut lines = vec![Line::from(""); (target.height.saturating_sub(2) / 2) as usize];
        lines.push(Line::from("♫").fg(DIM));
        lines.push(Line::from(caption).fg(DIM));
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), target);
    }
}

fn draw_info(frame: &mut Frame, app: &mut App, area: Rect) {
    let lines = match app.now_playing() {
        Some(t) => vec![
            Line::from(Span::styled(
                t.title.clone(),
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(t.artist.clone(), Style::new().fg(ACCENT))),
            Line::from(Span::styled(t.album.clone(), Style::new().fg(DIM))),
        ],
        None => vec![
            Line::from(Span::styled(
                "Nothing playing",
                Style::new().fg(DIM).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "pick a track and hit Enter",
                Style::new().fg(DIM),
            )),
        ],
    };

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_controls(frame: &mut Frame, app: &mut App, area: Rect) {
    let [progress, _gap, buttons] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(area);

    draw_progress(frame, app, progress);
    draw_transport(frame, app, buttons);
}

fn draw_progress(frame: &mut Frame, app: &mut App, area: Rect) {
    let pos = app.audio.position();
    let total = app.now_playing().and_then(|t| t.duration);

    // Keep the label in its own rect so `seek_bar` is exactly the clickable bar;
    // LineGauge's own label would eat into it by an amount we cannot measure.
    let label = match (app.is_playing_something(), total) {
        (true, Some(total)) => format!("{} / {} ", fmt_duration(pos), fmt_duration(total)),
        (true, None) => format!("{} ", fmt_duration(pos)),
        _ => "--:-- / --:-- ".into(),
    };
    let [label_area, bar] = Layout::horizontal([
        Constraint::Length(label.chars().count() as u16),
        Constraint::Min(0),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, Style::new().fg(DIM)))),
        label_area,
    );

    let ratio = match total {
        Some(total) if total.as_secs_f64() > 0.0 && app.is_playing_something() => {
            (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    frame.render_widget(
        LineGauge::default()
            .ratio(ratio)
            .label("")
            .filled_style(Style::new().fg(ACCENT))
            .unfilled_style(Style::new().fg(DIM)),
        bar,
    );
    app.seek_bar = bar;
}

fn draw_transport(frame: &mut Frame, app: &mut App, area: Rect) {
    // Fixed-width side buttons, play takes the middle.
    let [prev, play, next] = Layout::horizontal([
        Constraint::Length(7),
        Constraint::Min(7),
        Constraint::Length(7),
    ])
    .areas(area);

    let playing = app.is_playing_something() && !app.audio.is_paused();
    let (glyph, text, color) = if playing {
        ("⏸", "Pause", ACCENT_ALT)
    } else {
        ("▶", "Play", ACCENT)
    };

    frame.render_widget(transport_button("⏮", DIM), prev);
    frame.render_widget(transport_button("⏭", DIM), next);
    frame.render_widget(transport_button(&format!("{glyph}  {text}"), color), play);

    // Remember where they landed so the mouse handler can hit-test them.
    app.prev_area = prev;
    app.play_area = play;
    app.next_area = next;
}

fn transport_button(text: &str, color: Color) -> Paragraph<'_> {
    Paragraph::new(Line::from(Span::styled(
        text.to_owned(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center)
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color)),
    )
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    // Kept terse on purpose: the line has to survive a narrow terminal without
    // truncating away `q quit`.
    let keys = [
        ("Tab", "pane"),
        ("↑↓", "move"),
        ("⏎", "open"),
        ("⌫", "up"),
        ("Space", "pause"),
        ("n/p", "track"),
        ("[/]", "seek"),
        ("+/-", "vol"),
        ("1/2", "source"),
        ("q", "quit"),
    ];
    let mut spans = Vec::new();
    for (key, desc) in keys {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().fg(Color::Rgb(30, 30, 46)).bg(DIM),
        ));
        spans.push(Span::styled(format!(" {desc} "), Style::new().fg(DIM)));
    }
    spans.push(Span::styled(
        format!("vol {:.0}%", app.audio.volume() * 100.0),
        Style::new().fg(DIM),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui_image::picker::Picker;

    use crate::app::App;

    fn app() -> App {
        App::new(
            PathBuf::from("."),
            Picker::halfblocks(),
            crate::media::Bridge::detached(),
        )
        .expect("app init")
    }

    #[test]
    fn renders_all_three_panes() {
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        let mut app = App::new(
            PathBuf::from(std::env::var("TUNETERM_ROOT").unwrap_or_else(|_| ".".into())),
            Picker::halfblocks(),
            crate::media::Bridge::detached(),
        )
        .expect("app init");
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();

        let rendered = format!("{}", terminal.backend());
        // The left pane title is a breadcrumb now, so it carries the folder name.
        for expected in [
            &app.here(),
            "Tracks",
            "Now Playing",
            "Play",
            "TITLE",
            "quit",
            "open",
        ] {
            assert!(rendered.contains(expected), "missing {expected:?}");
        }
    }

    /// The play button must land somewhere clickable.
    #[test]
    fn control_areas_are_recorded() {
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        let mut app = app();
        terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
        for (name, area) in [
            ("play", app.play_area),
            ("prev", app.prev_area),
            ("next", app.next_area),
            ("seek", app.seek_bar),
        ] {
            assert!(area.width > 0 && area.height > 0, "{name} not laid out");
        }
    }

    /// Regression: a short pane used to panic on `clamp(min, max)` with max < min.
    #[test]
    fn survives_tiny_terminals() {
        for (w, h) in [(20, 5), (40, 10), (8, 3), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut app = app();
            terminal
                .draw(|frame| super::draw(frame, &mut app))
                .unwrap_or_else(|e| panic!("{w}x{h} failed: {e}"));
        }
    }

    // `Picker::halfblocks` reports a 10x20 pixel cell, so 1 row == 2 columns.

    /// The art is laid out exactly as `draw_now_playing` does it.
    fn art(app: &App, cols: u16, rows: u16) -> Rect {
        let size = super::art_cells(app, cols, rows);
        super::centre_in(Rect::new(0, 0, cols, size.1), size)
    }

    /// A portrait cover must gain equal left and right margins — it used to be
    /// pinned to the left edge.
    #[test]
    fn tall_cover_is_centred_horizontally() {
        let mut app = app();
        app.cover_size = Some((200, 400));
        // 40 cols == 400 px wide, 10 rows == 200 px tall: height is the limit,
        // so 100x200 px == 10x10 cells get drawn.
        let rect = art(&app, 40, 10);
        assert_eq!((rect.width, rect.height), (10, 10), "size");
        assert_eq!(rect.x, (40 - 10) / 2, "left margin == right margin");
    }

    /// Regression: a 300 px cover in a wide pane hugged the left edge, because the
    /// rect handed to `StatefulImage` was the full pane width while the cover was
    /// only drawn at native size. The rect must now be the native size, centred —
    /// which also means no resize work at all.
    #[test]
    fn small_cover_keeps_native_size_and_is_centred() {
        let mut app = app();
        app.cover_size = Some((300, 300));
        // 80 cols == 800 px of room, but 300x300 px == 30x15 cells is what is drawn.
        let rect = art(&app, 80, 40);
        assert_eq!(
            (rect.width, rect.height),
            (30, 15),
            "native size, not enlarged"
        );
        assert_eq!(rect.x, (80 - 30) / 2, "centred horizontally");
    }

    /// A cover bigger than its box is shrunk to fit, never cropped.
    #[test]
    fn large_cover_is_shrunk_to_fit() {
        let mut app = app();
        app.cover_size = Some((3000, 3000));
        // Width limits at 400 px, so 400 px tall == 20 rows.
        let rect = art(&app, 40, 40);
        assert_eq!((rect.width, rect.height), (40, 20));
    }

    /// The reserved rows must equal the drawn rows, or the layout leaves a gap.
    /// This caught a `ceil` vs `round` mismatch that showed up only for some sizes.
    #[test]
    fn reserved_height_always_matches_drawn_height() {
        let mut app = app();
        for size in [(300, 300), (1400, 1400), (600, 900), (900, 600), (1, 5000)] {
            app.cover_size = Some(size);
            for (cols, rows) in [(40u16, 42u16), (30, 15), (7, 3), (200, 58)] {
                let computed = super::art_cells(&app, cols, rows);
                let rect = art(&app, cols, rows);
                assert_eq!(
                    rect.height, computed.1,
                    "cover {size:?} in {cols}x{rows}: reserved {} drawn {}",
                    computed.1, rect.height
                );
            }
        }
    }

    #[test]
    fn art_rect_never_escapes_its_box() {
        for size in [(1u32, 20u32), (200, 400), (300, 300), (4000, 100), (50, 50)] {
            for (cols, rows) in [(1u16, 1u16), (3, 2), (40, 10), (200, 60)] {
                let mut app = app();
                app.cover_size = Some(size);
                let box_rect = Rect::new(0, 0, cols, rows);
                let rect = super::centre_in(box_rect, super::art_cells(&app, cols, rows));
                assert!(
                    rect.right() <= box_rect.right() && rect.bottom() <= box_rect.bottom(),
                    "cover {size:?} in {cols}x{rows} produced {rect:?}"
                );
            }
        }
    }

    #[test]
    fn centre_in_splits_slack_evenly() {
        let rect = super::centre_in(Rect::new(5, 9, 21, 11), (11, 5));
        assert_eq!(rect, Rect::new(5 + 5, 9 + 3, 11, 5));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::{Picker, ProtocolType};
    use std::time::Instant;

    /// `TUNETERM_FILE=... cargo test bench_render -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_render() {
        let path = std::env::var("TUNETERM_FILE").expect("set TUNETERM_FILE");
        let img = crate::library::load_cover(std::path::Path::new(&path)).expect("cover");
        println!(
            "cover {}x{}   profile: {}",
            img.width(),
            img.height(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );

        let cases: [(&str, Resize); 5] = [
            ("Nearest", Resize::Scale(Some(FilterType::Nearest))),
            ("Triangle", Resize::Scale(Some(FilterType::Triangle))),
            ("CatmullRom", Resize::Scale(Some(FilterType::CatmullRom))),
            ("Lanczos3", Resize::Scale(Some(FilterType::Lanczos3))),
            // What the app actually does: never enlarge, so nothing to resize.
            ("APP Fit/Triangle", Resize::Fit(Some(ART_FILTER))),
        ];

        for (name, resize) in cases {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(ProtocolType::Kitty);
            let mut app = App::new(
                std::path::PathBuf::from("."),
                picker,
                crate::media::Bridge::detached(),
            )
            .expect("app");
            app.cover_size = Some((img.width(), img.height()));
            app.cover = Some(app.picker.new_resize_protocol(img.clone()));

            // 60x30 cells at 10x20 px == 600x600 px target: the real-world case.
            let area = Rect::new(0, 0, 60, 30);
            let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
            let render = |terminal: &mut Terminal<TestBackend>, app: &mut App| {
                let r = resize.clone();
                terminal
                    .draw(|f| {
                        let p = app.cover.as_mut().unwrap();
                        f.render_stateful_widget(StatefulImage::default().resize(r), area, p);
                    })
                    .unwrap();
            };

            let t = Instant::now();
            render(&mut terminal, &mut app);
            let first = t.elapsed();

            let t = Instant::now();
            for _ in 0..10 {
                render(&mut terminal, &mut app);
            }
            println!(
                "  {:<12} first {:>9.1?}   steady {:>9.1?}",
                name,
                first,
                t.elapsed() / 10
            );
        }
    }

    /// How the whole cover pipeline scales with source size: `cargo test bench_sizes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_sizes() {
        use image::{DynamicImage, RgbImage};
        println!(
            "profile: {}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        for side in [300u32, 600, 1000, 1400, 3000] {
            let img = DynamicImage::ImageRgb8(RgbImage::from_fn(side, side, |x, y| {
                image::Rgb([(x % 255) as u8, (y % 255) as u8, 128])
            }));
            // Encode to jpeg so the decode cost is realistic too.
            let mut jpeg = std::io::Cursor::new(Vec::new());
            img.write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
            let bytes = jpeg.into_inner();

            let t = Instant::now();
            let decoded = image::load_from_memory(&bytes).unwrap();
            let decode = t.elapsed();

            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(ProtocolType::Kitty);
            let mut app = App::new(
                std::path::PathBuf::from("."),
                picker,
                crate::media::Bridge::detached(),
            )
            .expect("app");
            app.cover_size = Some((side, side));
            app.cover = Some(app.picker.new_resize_protocol(decoded));

            // A realistic pane: 30x15 cells == 300x300 px.
            let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
            let t = Instant::now();
            terminal
                .draw(|f| draw_art(f, &mut app, Rect::new(0, 0, 30, 15)))
                .unwrap();
            let render = t.elapsed();
            println!(
                "  {side:>4}px jpeg {:>5}KB   decode {:>8.1?}   render {:>8.1?}   total {:>8.1?}",
                bytes.len() / 1024,
                decode,
                render,
                decode + render
            );
        }
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use crate::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::Picker;

    /// `TUNETERM_ROOT=... cargo test preview_panes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn preview_panes() {
        let root = std::env::var("TUNETERM_ROOT").expect("set TUNETERM_ROOT");
        let mut app = App::new(
            std::path::PathBuf::from(root),
            Picker::halfblocks(),
            crate::media::Bridge::detached(),
        )
        .unwrap();
        app.wait_for_tracks();

        for (label, step) in [
            ("at the root", 0),
            ("inside a folder", 1),
            ("the radio tab", 2),
        ] {
            match step {
                1 => {
                    app.enter_folder();
                    app.wait_for_tracks();
                }
                2 => app.select_tab(crate::app::Tab::Radio),
                _ => {}
            }
            let mut terminal = Terminal::new(TestBackend::new(78, 14)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            println!("\n--- {label} ---");
            for line in format!("{}", terminal.backend()).lines() {
                let chars: Vec<char> = line.chars().collect();
                let cut: String = chars[..chars.len().min(58)].iter().collect();
                println!("{cut}");
            }
        }
    }
}
