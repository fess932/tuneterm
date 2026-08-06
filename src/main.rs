mod app;
mod cache;
mod cover;
mod library;
mod player;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::Position;
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};

use app::{App, Pane};

fn music_root() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1).filter(|a| !a.starts_with("--")) {
        return PathBuf::from(arg);
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    // Apple Music keeps its library a few levels down; prefer it when present.
    let itunes = home.join("Music/Music/Media.localized/Music");
    if itunes.is_dir() {
        itunes
    } else {
        home.join("Music")
    }
}

/// Choose a graphics protocol without talking to the terminal.
///
/// `Picker::from_query_stdio` writes capability queries and reads the replies on a
/// detached thread (ratatui-image 11.0.6, `picker.rs::query_with_timeout`). When
/// the terminal never replies, the main thread gives up on its timeout but that
/// thread stays blocked in `io::stdin().read()` forever, and from then on it races
/// crossterm for every keystroke — the app silently stops responding to the
/// keyboard. Proxied terminals and multiplexers hit this, so by default we derive
/// everything locally. `TUNETERM_QUERY=1` opts into the accurate detection when you
/// know your terminal answers.
fn build_picker() -> Picker {
    if std::env::var_os("TUNETERM_QUERY").is_some()
        && let Ok(picker) = Picker::from_query_stdio()
    {
        return picker;
    }

    #[allow(deprecated)] // the only constructor that takes a known font size
    let mut picker = Picker::from_fontsize(cell_size());
    if let Some(protocol) = protocol_from_env() {
        picker.set_protocol_type(protocol);
    }
    picker
}

/// Cell size in pixels, straight from TIOCGWINSZ — an ioctl, so no round-trip.
fn cell_size() -> FontSize {
    const FALLBACK: (u16, u16) = (10, 20);
    let Ok(ws) = terminal::window_size() else {
        return FALLBACK.into();
    };
    if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
        // Terminal does not report pixel dimensions.
        return FALLBACK.into();
    }
    (ws.width / ws.columns, ws.height / ws.rows).into()
}

fn protocol_from_env() -> Option<ProtocolType> {
    let set = |key: &str| std::env::var_os(key).is_some();
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();

    // Kitty protocol — the only one supporting unicode placeholders.
    if set("KITTY_WINDOW_ID")
        || set("GHOSTTY_RESOURCES_DIR")
        || set("GHOSTTY_BIN_DIR")
        || term_program == "ghostty"
        || term.contains("kitty")
        || term.contains("ghostty")
    {
        return Some(ProtocolType::Kitty);
    }
    // WezTerm's kitty support lacks placeholders; its iTerm2 path is the solid one.
    if set("WEZTERM_PANE") || set("WEZTERM_EXECUTABLE") || term_program == "WezTerm" {
        return Some(ProtocolType::Iterm2);
    }
    if set("ITERM_SESSION_ID")
        || set("KONSOLE_VERSION")
        || matches!(term_program.as_str(), "iTerm.app" | "mintty")
    {
        return Some(ProtocolType::Iterm2);
    }
    if ["foot", "contour", "mlterm", "sixel"]
        .iter()
        .any(|t| term.contains(t))
    {
        return Some(ProtocolType::Sixel);
    }
    None // halfblocks
}

/// Headless dump of what the scanner sees. Handy because a TUI hides errors.
fn scan_report(root: &std::path::Path) {
    let folders = library::scan_folders(root, 5);
    println!("root: {}", root.display());
    println!("folders: {}", folders.len());
    for folder in folders.iter().take(10) {
        println!("  {} ({} files)", folder.label, folder.count);
    }
    if let Some(first) = folders.first() {
        let tracks = library::scan_tracks(&first.path);
        println!("\ntracks in \"{}\": {}", first.label, tracks.len());
        for track in tracks.iter().take(5) {
            println!(
                "  {} — {} [{}] {}",
                track.artist,
                track.title,
                track.album,
                track
                    .duration
                    .map(library::fmt_duration)
                    .unwrap_or_else(|| "--:--".into())
            );
        }
        println!("\ncovers:");
        for track in tracks.iter().take(8) {
            let name = track
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match library::load_cover(&track.path) {
                Some(img) => println!("  {}x{}  {}", img.width(), img.height(), name),
                None => println!("  none       {}", name),
            }
        }
    }
}

fn main() -> Result<()> {
    let root = music_root();
    if std::env::args().any(|a| a == "--scan") {
        scan_report(&root);
        return Ok(());
    }

    // Must query the terminal before we switch to the alternate screen.
    let picker = build_picker();

    let mut app = App::new(root, picker)?;

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;

    let result = run(&mut terminal, &mut app);

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        // Poll so the progress bar keeps ticking while idle.
        let ready = event::poll(Duration::from_millis(120))?;
        if ready {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => on_key(app, key),
                Event::Mouse(mouse) => on_mouse(app, mouse),
                _ => {}
            }
        }
        app.poll_cover();
        app.refresh_cover_for_resize();
        app.tick();
    }
    Ok(())
}

fn on_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab | KeyCode::BackTab => app.focus_next(),
        KeyCode::Left | KeyCode::Char('h') => app.focus = Pane::Folders,
        KeyCode::Right | KeyCode::Char('l') => app.focus = Pane::Tracks,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Enter => match app.focus {
            // Folder contents already load on selection; Enter jumps to them.
            Pane::Folders => app.focus = Pane::Tracks,
            Pane::Tracks => app.play_selected_track(),
        },
        KeyCode::Char(' ') => app.toggle_play(),
        KeyCode::Char('n') => app.next_track(),
        KeyCode::Char('p') => app.prev_track(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.audio.nudge_volume(0.05),
        KeyCode::Char('-') => app.audio.nudge_volume(-0.05),
        KeyCode::Char('[') => app.seek_by(-5),
        KeyCode::Char(']') => app.seek_by(5),
        _ => {}
    }
}

fn on_mouse(app: &mut App, mouse: MouseEvent) {
    let pos = Position {
        x: mouse.column,
        y: mouse.row,
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => app.click(pos, Instant::now()),
        MouseEventKind::Drag(MouseButton::Left) => app.drag(pos),
        MouseEventKind::ScrollDown => app.scroll(pos, 1),
        MouseEventKind::ScrollUp => app.scroll(pos, -1),
        _ => {}
    }
}
