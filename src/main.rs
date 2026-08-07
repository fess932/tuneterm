mod app;
mod cache;
mod config;
mod cover;
mod feed;
mod library;
mod media;
mod net;
mod player;
mod ui;
mod worker;

use std::io::{self, Write, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::terminal;
use ratatui::layout::Position;
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};

use app::{App, Pane};

/// Parsed command line. Hand-rolled: three flags do not justify a dependency.
struct Args {
    root: Option<PathBuf>,
    scan: bool,
    media: bool,
}

/// `Err` carries the message to print; the caller decides the exit code.
#[derive(Debug)]
enum ArgsError {
    /// Printed to stdout, exit 0 — the user asked for it.
    Handled(String),
    /// Printed to stderr, exit 2.
    Bad(String),
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, ArgsError> {
    let mut parsed = Args {
        root: None,
        scan: false,
        media: true,
    };
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Err(ArgsError::Handled(help())),
            "-V" | "--version" => {
                return Err(ArgsError::Handled(format!(
                    "{} {}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )));
            }
            "--scan" => parsed.scan = true,
            "--no-media" => parsed.media = false,
            // Catch typos instead of silently launching with a default library.
            other if other.starts_with('-') => {
                return Err(ArgsError::Bad(format!(
                    "unknown option: {other}\n\nTry --help."
                )));
            }
            path if parsed.root.is_none() => parsed.root = Some(PathBuf::from(path)),
            extra => {
                return Err(ArgsError::Bad(format!(
                    "unexpected argument: {extra}\n\nOnly one folder can be given."
                )));
            }
        }
    }
    Ok(parsed)
}

fn help() -> String {
    format!(
        "\
{name} {version} — {about}

USAGE
    {name} [FOLDER] [OPTIONS]

ARGS
    FOLDER            Music folder to browse. Defaults to the Apple Music
                      library if present, otherwise ~/Music.

OPTIONS
    --scan            Print folders, tags and cover sizes, then exit. A TUI
                      hides errors; use this to check scanning works.
    --no-media        Skip the OS media-key integration.
    -h, --help        Print this help.
    -V, --version     Print the version.

SOURCES
    Tabs in the top border of the browsing pane. Click one or press 1 - 3;
    switching never stops playback.

    Local browses the filesystem. Feeds plays podcast and mixtape RSS: pick a
    feed on the left, its episodes list on the right, Enter streams one. The Add
    feed row, or `a`, opens a field for a URL; `d` or the x removes the
    highlighted entry. The list lives in feeds.txt in the config directory.
    Radio is a placeholder.

BROWSING
    The left pane is a folder browser. Moving the cursor lists everything in the
    highlighted folder *and its subfolders* on the right, so an artist shows their
    whole discography. Enter descends, Backspace goes back up.

KEYS
    Tab, h/l, arrows  Switch pane            Space         Play / pause
    j/k, PgUp/PgDn    Move selection         n / p         Next / previous
    Enter             Open folder / play     [ / ]         Seek -/+ 5s
    Backspace         Go up a folder         + / -         Volume
    1 - 3             Switch source
    q, Esc, Ctrl-C    Quit

    The mouse works too: click a row to select, double-click to play, click the
    transport buttons, and click or drag the progress bar to seek.

    Media keys, headphone buttons, Control Center and MPRIS control playback
    even while the terminal is in the background.

ENVIRONMENT
    TUNETERM_CACHE_DIR   Cache root. Defaults to the platform cache directory,
                         with art capped at 200 MB and audio at 2000 MB.
    TUNETERM_CONFIG_DIR  Where feeds.txt lives. Defaults to the platform
                         config directory.
    TUNETERM_QUERY=1     Ask the terminal which graphics protocol it supports
                         instead of guessing from the environment. More
                         accurate, but a terminal that never answers leaves the
                         app unable to read the keyboard.

{repo}",
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        about = env!("CARGO_PKG_DESCRIPTION"),
        repo = env!("CARGO_PKG_REPOSITORY"),
    )
}

fn default_root() -> PathBuf {
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
///
/// Writes through a locked handle and stops on the first write error, so piping
/// into `head` closes the pipe quietly instead of panicking in `println!`.
fn scan_report(root: &Path) -> io::Result<()> {
    let stdout = io::stdout();
    let out = &mut stdout.lock();

    let folders = library::list_subdirs(root);
    writeln!(out, "root: {}", root.display())?;
    writeln!(out, "subfolders: {}", folders.len())?;
    for folder in folders.iter().take(10) {
        writeln!(out, "  {} ({} files)", folder.label, folder.count)?;
    }

    // Everything below the first subfolder, or the root itself when it has none.
    let (label, dir) = match folders.first() {
        Some(folder) => (folder.label.clone(), folder.path.clone()),
        None => (root.display().to_string(), root.to_path_buf()),
    };
    let tracks = library::scan_tracks_deep(&dir);
    writeln!(out, "\ntracks under \"{label}\": {}", tracks.len())?;
    for track in tracks.iter().take(5) {
        writeln!(
            out,
            "  {} — {} [{}] {}",
            track.artist,
            track.title,
            track.album,
            track
                .duration
                .map(library::fmt_duration)
                .unwrap_or_else(|| "--:--".into())
        )?;
    }

    writeln!(out, "\ncovers:")?;
    for track in tracks.iter().take(8) {
        let name = track
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match library::load_cover(&track.path) {
            Some(img) => writeln!(out, "  {}x{}  {}", img.width(), img.height(), name)?,
            None => writeln!(out, "  none       {name}")?,
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(ArgsError::Handled(text)) => {
            println!("{text}");
            return Ok(());
        }
        Err(ArgsError::Bad(text)) => {
            eprintln!("{text}");
            std::process::exit(2);
        }
    };

    // One-off: entries predate the art/audio split and are now unreachable.
    cache::tidy();

    let root = args.root.unwrap_or_else(default_root);
    if args.scan {
        // A closed pipe (`| head`) is not a failure worth reporting.
        return match scan_report(&root) {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            other => other.map_err(Into::into),
        };
    }

    // Must query the terminal before we switch to the alternate screen.
    let picker = build_picker();

    // An escape hatch: if registering with the OS misbehaves, the player still works.
    let (bridge, host, media_warning) = if args.media {
        media::start()
    } else {
        (media::Bridge::detached(), None, None)
    };

    // macOS only delivers media keys to the *main* thread's run loop, so the TUI
    // moves to a worker and the main thread is left to service the OS. The app is
    // built inside that worker because cpal's audio stream is not `Send`.
    let finished = Arc::new(AtomicBool::new(false));
    let tui = {
        let finished = Arc::clone(&finished);
        std::thread::Builder::new()
            .name("tui".into())
            .spawn(move || {
                let result = tui_main(root, picker, bridge, media_warning);
                finished.store(true, Ordering::Release);
                // Cut the host's current wait short so quitting is immediate.
                media::wake();
                result
            })?
    };

    if let Some(mut host) = host {
        while !finished.load(Ordering::Acquire) {
            host.pump();
        }
    }

    match tui.join() {
        Ok(result) => result,
        // `ratatui::init` installs a panic hook that restores the terminal, so by
        // here the screen is already usable and only the report is left to make.
        Err(_) => anyhow::bail!("the interface thread panicked"),
    }
}

/// Ask for clicks and drags, but **not** bare pointer motion.
///
/// `crossterm::event::EnableMouseCapture` also turns on mode 1003, "report every
/// mouse movement", which this app never uses — `on_mouse` ignores `Moved`. It is
/// not free, though: moving the mouse while spinning the wheel floods the queue,
/// and the scroll events land behind the motion. Mode 1002 still reports motion
/// while a button is held, which is what scrubbing the seek bar needs.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

fn set_mouse(sequence: &str) -> io::Result<()> {
    let mut out = stdout();
    out.write_all(sequence.as_bytes())?;
    out.flush()
}

/// Everything that touches the terminal, on one thread.
fn tui_main(
    root: PathBuf,
    picker: Picker,
    bridge: media::Bridge,
    media_warning: Option<String>,
) -> Result<()> {
    let mut app = App::new(root, picker, bridge)?;
    if let Some(warning) = media_warning {
        app.status = warning;
    }

    let mut terminal = ratatui::init();
    set_mouse(MOUSE_ON)?;

    let result = run(&mut terminal, &mut app);

    let _ = set_mouse(MOUSE_OFF);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        // Idle at a lazy 120ms, but poll briskly while a cover is in flight: the
        // worker cannot wake this loop, so the timeout is what delays the artwork.
        let timeout = if app.cover_pending {
            Duration::from_millis(15)
        } else {
            Duration::from_millis(120)
        };
        // Wait for the first event, then take everything already queued.
        //
        // Reading a single event per iteration capped input at the frame rate. A
        // fast scroll wheel outruns that easily, so events piled up in the
        // terminal's buffer and kept moving the cursor the old way for a while
        // after you had already reversed direction — the reversal was simply
        // queued behind the backlog. Draining collapses a flick into one frame.
        if event::poll(timeout)? {
            // Bounded so a stream of events cannot starve the redraw entirely.
            const MAX_PER_FRAME: usize = 256;
            for _ in 0..MAX_PER_FRAME {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => on_key(app, key),
                    Event::Mouse(mouse) => on_mouse(app, mouse),
                    _ => {}
                }
                if app.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.poll_cover();
        app.poll_tracks();
        app.poll_feed();
        app.refresh_cover_for_resize();
        app.poll_media();
        app.tick();
        app.publish_now_playing();
    }
    Ok(())
}

fn on_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    // While an input is open every key belongs to it, or typing a URL containing
    // "q" would quit and "n" would skip a track.
    if app.prompt.is_some() {
        match key.code {
            KeyCode::Esc => app.cancel_prompt(),
            KeyCode::Enter => app.submit_prompt(),
            KeyCode::Backspace => app.prompt_backspace(),
            KeyCode::Char(c) => {
                app.prompt_key(c);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab | KeyCode::BackTab => app.focus_next(),
        KeyCode::Left | KeyCode::Char('h') => app.focus = Pane::Folders,
        KeyCode::Right | KeyCode::Char('l') => app.focus = Pane::Tracks,
        KeyCode::Down | KeyCode::Char('j') if app.tab == app::Tab::Feeds => {
            app.move_feed_selection(1);
        }
        KeyCode::Up | KeyCode::Char('k') if app.tab == app::Tab::Feeds => {
            app.move_feed_selection(-1);
        }
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Enter => match app.focus {
            // The track list already follows the cursor, so Enter is for descending.
            Pane::Folders => app.enter_folder(),
            Pane::Tracks => app.play_selected_track(),
        },
        KeyCode::Backspace => app.leave_folder(),
        KeyCode::Char(' ') => app.toggle_play(),
        KeyCode::Char('n') => app.next_track(),
        KeyCode::Char('p') => app.prev_track(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.audio.nudge_volume(0.05),
        KeyCode::Char('-') => app.audio.nudge_volume(-0.05),
        KeyCode::Char('a') if app.tab == app::Tab::Feeds => app.open_add_feed(),
        KeyCode::Char('d') if app.tab == app::Tab::Feeds => app.remove_selected_feed(),
        KeyCode::Char('1') => app.select_tab(app::Tab::Local),
        KeyCode::Char('2') => app.select_tab(app::Tab::Feeds),
        KeyCode::Char('3') => app.select_tab(app::Tab::Radio),
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

#[cfg(test)]
mod args_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, ArgsError> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn defaults_to_no_folder_and_media_on() {
        let args = parse(&[]).expect("empty is valid");
        assert!(args.root.is_none());
        assert!(!args.scan);
        assert!(args.media, "media integration is on unless refused");
    }

    #[test]
    fn takes_a_folder_and_flags_in_any_order() {
        for order in [
            vec!["--scan", "/music", "--no-media"],
            vec!["/music", "--no-media", "--scan"],
            vec!["--no-media", "--scan", "/music"],
        ] {
            let args = parse(&order).unwrap_or_else(|_| panic!("{order:?}"));
            assert_eq!(args.root.as_deref(), Some(Path::new("/music")), "{order:?}");
            assert!(args.scan, "{order:?}");
            assert!(!args.media, "{order:?}");
        }
    }

    #[test]
    fn help_and_version_are_handled_not_errors() {
        for flag in ["-h", "--help"] {
            match parse(&[flag]) {
                Err(ArgsError::Handled(text)) => {
                    assert!(text.contains("USAGE"), "{flag}");
                    assert!(text.contains("--scan"), "{flag} omits a real flag");
                }
                _ => panic!("{flag} should be handled"),
            }
        }
        for flag in ["-V", "--version"] {
            match parse(&[flag]) {
                Err(ArgsError::Handled(text)) => {
                    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{flag}");
                }
                _ => panic!("{flag} should be handled"),
            }
        }
    }

    /// A typo must not silently launch against the default library.
    #[test]
    fn rejects_unknown_options() {
        for bad in ["--scna", "--help=1", "-x"] {
            match parse(&[bad]) {
                Err(ArgsError::Bad(text)) => assert!(text.contains(bad), "{bad}"),
                _ => panic!("{bad} should be rejected"),
            }
        }
    }

    #[test]
    fn rejects_a_second_folder() {
        match parse(&["/one", "/two"]) {
            Err(ArgsError::Bad(text)) => assert!(text.contains("/two")),
            _ => panic!("two folders should be rejected"),
        }
    }

    /// Every option the help text advertises must actually parse.
    #[test]
    fn help_does_not_advertise_options_that_do_not_exist() {
        let text = help();
        for flag in ["--scan", "--no-media", "--help", "--version"] {
            assert!(text.contains(flag), "help omits {flag}");
            assert!(
                !matches!(parse(&[flag]), Err(ArgsError::Bad(_))),
                "help advertises {flag} but parsing rejects it"
            );
        }
    }
}
