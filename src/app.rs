use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::config::{self, Feed};
use crate::cover::{self, CoverLoader};
use crate::library::{self, Folder, Stamp, Track};
use crate::media::{self, Command, NowPlaying};
use crate::player::{self, AudioPlayer};
use crate::remote;
use crate::worker::{Cancel, Wake, Worker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Folders,
    Tracks,
}

/// Where music comes from. Only [`Tab::Local`] does anything yet; see PLAN.md for
/// what the others would take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Local,
    Feeds,
    Radio,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Local, Tab::Feeds, Tab::Radio];

    /// How the tab is written in `settings.txt`. Not the label: that is on screen
    /// and can be reworded, while this is on disk and has to keep meaning the same.
    pub fn key(self) -> &'static str {
        match self {
            Tab::Local => "local",
            Tab::Feeds => "feeds",
            Tab::Radio => "radio",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Tab::ALL.into_iter().find(|tab| tab.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Local => "Local",
            Tab::Feeds => "Feeds",
            Tab::Radio => "Radio",
        }
    }

    /// Shown in place of the pane while a source is still a plan.
    pub fn placeholder(self) -> &'static [&'static str] {
        match self {
            Tab::Local => &[],
            Tab::Feeds => &["pick a feed on the left"],
            Tab::Radio => &["not built yet", "", "see PLAN.md"],
        }
    }
}

pub struct App {
    pub root: PathBuf,
    /// Which source is on screen.
    pub tab: Tab,
    /// Directory the left pane is listing. Never climbs above `root`.
    pub cwd: PathBuf,
    /// Subdirectories of `cwd`. A cursor here drives the track list.
    pub folders: Vec<Folder>,
    /// Selection to restore when stepping back up, per directory left behind.
    trail: Vec<(PathBuf, usize)>,
    pub tracks: Vec<Track>,
    pub folder_state: TableState,
    pub track_state: TableState,
    pub focus: Pane,

    /// Snapshot of the listing taken when playback started, plus where in it we are.
    ///
    /// Playback used to be an index into `tracks`, which broke the moment browsing
    /// started rebuilding that list under the cursor: next and previous would walk
    /// whatever folder you happened to be hovering.
    queue: Vec<Track>,
    queue_pos: Option<usize>,
    /// The folder the queue was listed from, so a restart can rebuild it even
    /// when the cursor has moved elsewhere since. `None` for a feed.
    queue_dir: Option<PathBuf>,
    /// Directory whose tracks are listed, and whether the worker is still on it.
    tracks_dir: Option<PathBuf>,
    pub tracks_loading: bool,
    /// Hands back the directory it scanned, so a result can never be filed under
    /// whatever the cursor has moved to meanwhile.
    scan: Worker<Listing, Listed>,
    /// Lists the folder playback runs on into once the queue is used up. Its own
    /// worker, so that a cursor moving meanwhile cannot supersede it.
    follow: Worker<Listing, Listed>,
    follow_generation: u64,
    /// The folder being listed for that, while it is.
    following_into: Option<PathBuf>,
    /// Opens remote streams, which blocks for as long as the server takes. Hands
    /// back the URL for the same reason the scan hands back its directory.
    open: Worker<String, (String, Result<player::RemoteSource, String>)>,
    open_generation: u64,
    /// The URL being opened, if any.
    opening: Option<String>,
    scan_generation: u64,
    /// Recently listed directories, so moving back over a folder is instant — as
    /// long as it still looks the way it did when it was read.
    memo: HashMap<PathBuf, (Stamp, Vec<Track>)>,
    memo_order: VecDeque<PathBuf>,
    pub cover: Option<StatefulProtocol>,
    /// Pixel size of the cover as it will be drawn. Needed in full, not just as a
    /// ratio: a cover is never enlarged, so the drawn size — and therefore the
    /// centring — depends on the source size.
    pub cover_size: Option<(u32, u32)>,
    /// True between asking the worker for a cover and getting an answer.
    pub cover_pending: bool,
    /// Where the scaled cover landed on disk, for the OS "now playing" artwork.
    cover_file: Option<PathBuf>,
    /// The decoded cover kept in memory, with the folder and box it belongs to.
    ///
    /// Tracks of an album share a picture, so switching within one can reuse this
    /// straight away instead of waiting on the worker — which even on a cache hit
    /// costs a tag read, a PNG decode and a trip through the channel.
    cover_memo: Option<(PathBuf, (u32, u32), image::DynamicImage)>,
    cover_loader: CoverLoader,
    /// Bumped on every request so late replies can be recognised and dropped.
    cover_generation: u64,
    /// Box of the outstanding/last request, to notice when the pane changes size.
    cover_requested_box: (u32, u32),
    /// Largest box the art could occupy, in cells. Written during render; used to
    /// tell the worker how far to shrink.
    pub art_budget: Rect,
    pub picker: Picker,
    pub audio: AudioPlayer,

    /// Play the queue in a scrambled order rather than in listing order.
    pub shuffle: bool,
    /// The order `next`/`prev` follow while shuffling: a permutation of `queue`
    /// with whatever is playing at its head.
    ///
    /// A permutation rather than a fresh number per track, so the queue is heard
    /// once through instead of repeating at random, and so `prev` goes back to
    /// what was actually just played.
    shuffle_order: Vec<usize>,
    rng: Rng,

    /// Where the settings are written, held rather than looked up for the same
    /// reason as `feeds_file`: tests must not touch the user's real file.
    pub settings_file: Option<PathBuf>,
    /// The server whose folders are listed beside the local ones at the root, and
    /// its token. Added with `a`, kept in the settings.
    pub remote: config::Remote,
    /// The last folder sent to the server — on its way, or done and its log still
    /// on screen.
    transfer: Option<Transfer>,
    /// Handed to the move's thread, so progress reaches the screen at once.
    wake: Wake,
    /// When the session last changed, if it has not been written out yet.
    settings_dirty: Option<Instant>,
    /// The playhead as last written, so `tick` can tell how far it has drifted.
    saved_position: Duration,
    /// The track the last session was on, until the first listing has had a chance
    /// to contain it. One shot: see [`App::try_resume`].
    resume: Option<Resume>,
    /// Where to put the playhead once that track is actually queued, which for a
    /// stream is several seconds after we ask for it — and which track that is,
    /// since by then it may not be the one still wanted.
    resume_at: Option<(String, Duration)>,
    /// Set while a restored track is held quiet waiting for its seek to land, so
    /// that [`App::poll_seek`] knows to let it go when the answer arrives.
    resume_playing: bool,

    /// Written during render so mouse events can hit-test. `*_rows` cover only
    /// the data rows of each table — no border, no header — and `seek_bar` only
    /// the gauge itself, not the time label beside it.
    pub play_area: Rect,
    pub prev_area: Rect,
    pub next_area: Rect,
    pub seek_bar: Rect,
    /// The shuffle button in the key bar, for hit-testing.
    pub shuffle_area: Rect,
    pub folder_rows: Rect,
    pub track_rows: Rect,
    /// Clickable strip per tab, written during render. Same reason as the others:
    /// the layout knows where things landed, the event handler does not.
    pub tab_areas: [Rect; Tab::ALL.len()],

    /// The user's feed list, and where the cursor is in it.
    pub feeds: Vec<Feed>,
    pub feed_state: TableState,
    /// The `+ Add feed` button and the `✕` on the highlighted row, for hit-testing.
    pub add_area: Rect,
    pub remove_area: Rect,
    pub feed_rows: Rect,
    /// Open input, drawn over everything else.
    pub prompt: Option<Prompt>,
    /// Fetching and parsing a feed, off the render thread for the same reason a deep
    /// folder scan is: it is slow and it is driven by a moving cursor.
    fetch: Worker<String, Result<crate::feed::Channel, String>>,
    fetch_generation: u64,
    /// The feed whose episodes are listed, and whether it is still arriving.
    fetched_url: Option<String>,
    pub feed_loading: bool,
    /// Where the feed list is written. Held rather than looked up each time so tests
    /// can point it somewhere harmless instead of the user's real config.
    pub feeds_file: Option<PathBuf>,
    /// Pane, row and time of the last left click, for double-click detection.
    last_click: Option<(Pane, usize, Instant)>,

    /// Media keys and the OS "now playing" panel.
    media: media::Bridge,
    /// Last state handed to the OS, so we only publish on a real change.
    published: Option<NowPlaying>,

    pub status: String,
    pub should_quit: bool,
    /// The key list, drawn over everything until the next key.
    pub show_keys: bool,

    /// The three stars under the cover, for hit-testing. Zero while nothing plays.
    pub star_areas: [Rect; 3],
    /// Screen columns of the stars in the track list, one per star.
    pub track_star_x: Option<u16>,
}

/// A floating one-line input. Opened by the Add button, closed by Enter or Escape.
///
/// Kept as state rather than a blocking read so the rest of the app keeps running
/// behind it: the music plays, the cover arrives, the progress bar moves.
pub struct Prompt {
    pub kind: PromptKind,
    pub title: &'static str,
    pub input: String,
    /// Shown under the field: usage, or why the last attempt was refused.
    pub hint: String,
    /// An input that already failed a check the user may overrule: Enter on it
    /// again goes ahead anyway.
    pub retry: Option<String>,
}

/// What Enter in the prompt does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Feed,
    Server,
    /// A new name for this folder or track, in the same place.
    Rename(PathBuf),
    /// A new path for this folder or track, from the top of its library.
    Relocate(PathBuf),
    /// Waiting for `y` before deleting this folder or track.
    Delete(PathBuf),
}

/// A folder to list, and — for `..`, which lists the folder being browsed — its
/// folders one by one, with the tracks of those already read and unchanged since.
///
/// So listing everything costs only the folders never looked at or changed since,
/// and at the root takes in the server's the same way as the local ones: the
/// root's folder list already holds both, each folder once.
pub struct Listing {
    dir: PathBuf,
    /// How `dir` looked in its parent's list, to file the answer under. `None` for
    /// a listing that is not remembered.
    stamp: Option<Stamp>,
    parts: Vec<(PathBuf, Stamp, Option<Vec<Track>>)>,
}

/// What a [`Listing`] came back with: the tracks, and the folders it had to read on
/// the way, for the memo.
pub struct Listed {
    dir: PathBuf,
    stamp: Option<Stamp>,
    tracks: Result<Vec<Track>, String>,
    found: Vec<(PathBuf, Stamp, Vec<Track>)>,
}

impl Listing {
    fn run(self, cancel: &Cancel) -> Listed {
        if self.parts.is_empty() {
            let tracks = library::tracks(&self.dir, cancel);
            return Listed {
                dir: self.dir,
                stamp: self.stamp,
                tracks,
                found: Vec::new(),
            };
        }
        // The folder's own files first, the way a folder's own files come first.
        let mut tracks = library::scan_tracks(&self.dir);
        let mut found = Vec::new();
        for (path, stamp, known) in self.parts {
            match known {
                Some(known) => tracks.extend(known),
                // A server that stopped answering costs its own folders only.
                None => {
                    if let Ok(listed) = library::tracks(&path, cancel) {
                        tracks.extend(listed.iter().cloned());
                        found.push((path, stamp, listed));
                    }
                }
            }
        }
        Listed {
            dir: self.dir,
            stamp: self.stamp,
            tracks: Ok(tracks),
            found,
        }
    }
}

/// Stars as they are drawn: filled for what was given, hollow for the rest.
pub fn stars_text(stars: Option<u8>) -> String {
    let given = usize::from(stars.unwrap_or(0));
    "★".repeat(given) + &"☆".repeat(3 - given)
}

/// One line of a move's log: something that went wrong, or how it ended. Files
/// that arrived are not listed — the bar counts them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    Failed(String),
    Note(String),
}

/// A folder on its way to the server, as its thread reports it.
#[derive(Debug)]
struct Move {
    title: String,
    /// Where the folder goes on the server; stripped from names in the log, which
    /// are all inside it.
    base: String,
    files: usize,
    bytes: u64,
    done_files: usize,
    /// Bytes of files the server already had: done, but never sent.
    skipped_bytes: u64,
    /// The files being sent, several at once: name, size, and each one's own
    /// byte counter. A file leaves when it is done; the log is not a record of
    /// every file, only of what is happening and what went wrong.
    sending: Vec<(String, u64, Arc<AtomicU64>)>,
    log: VecDeque<LogLine>,
    started: Instant,
    /// Set once the thread is finished: what to say about it.
    finished: Option<String>,
    /// It finished without moving everything.
    failed: bool,
}

impl Move {
    /// Enough to scroll back through a big album; older lines are dropped.
    const KEEP: usize = 500;

    fn note(&mut self, line: LogLine) {
        self.log.push_back(line);
        while self.log.len() > Self::KEEP {
            self.log.pop_front();
        }
    }

    fn name(&self, remote: &str) -> String {
        remote
            .strip_prefix(&self.base)
            .map(|rest| rest.trim_start_matches('/'))
            .filter(|rest| !rest.is_empty())
            .unwrap_or(remote)
            .to_string()
    }
}

/// A move, and whether its log is on screen.
struct Transfer {
    state: Arc<Mutex<Move>>,
    /// Bytes sent so far, counted by the upload itself.
    uploaded: Arc<AtomicU64>,
    visible: bool,
    /// The library has been listed again after it finished.
    settled: bool,
}

/// What the log panel shows, taken in one go so the drawing never holds the lock.
pub struct TransferView {
    pub title: String,
    pub log: Vec<LogLine>,
    /// The files being sent: name, bytes sent, size.
    pub sending: Vec<(String, u64, u64)>,
    pub files_done: usize,
    pub files: usize,
    pub bytes_done: u64,
    pub bytes: u64,
    /// Bytes per second actually sent, skipped files left out.
    pub speed: f64,
    pub left: Option<Duration>,
    pub finished: Option<String>,
    pub failed: bool,
}

/// What was playing when the app was last closed, waiting for a listing to appear
/// that contains it.
struct Resume {
    /// Identity of the track, the way `Track` carries it: a path, or a URL.
    track: String,
    position: Duration,
    /// Whether it was playing rather than paused when the app was closed.
    playing: bool,
}

/// Where to put the playhead for a remembered track. Past the end of one that has
/// been re-encoded shorter, or simply finished last time, it starts again rather
/// than at a point that is not there any more.
fn resume_point(resume: &Resume, duration: Option<Duration>) -> Duration {
    match duration {
        Some(total) if resume.position >= total => Duration::ZERO,
        _ => resume.position,
    }
}

/// The last part of a path or an address, for showing.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Two clicks on the same row within this window count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// A xorshift. Shuffling is the only thing here that wants random numbers, and one
/// line of arithmetic is not worth a dependency.
struct Rng(u64);

impl Rng {
    /// Seeded from the clock: what is wanted is a different order each run, not a
    /// reproducible one.
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |since| since.as_nanos() as u64);
        // Any seed but zero; xorshift never leaves it.
        Self(nanos | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Below `n`. The modulo bias is one part in `2^64 / n`, which for a track
    /// listing is not something anyone can hear.
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

impl App {
    /// `wake` is what every worker rings when it has something, so the loop can
    /// wait instead of asking.
    pub fn new(root: PathBuf, picker: Picker, media: media::Bridge, wake: Wake) -> Result<Self> {
        let listed = library::subdirs(&root);
        let unreachable = listed.as_ref().err().cloned();
        let folders = listed.unwrap_or_default();
        // On the first folder rather than on `..`, which at the root would list —
        // and read the tags of — the whole library before anything was asked of it.
        let folder_state =
            TableState::default().with_selected(Some(usize::from(!folders.is_empty())));

        let mut app = Self {
            cwd: root.clone(),
            trail: Vec::new(),
            root,
            folders,
            tracks: Vec::new(),
            folder_state,
            track_state: TableState::default(),
            focus: Pane::Folders,
            queue: Vec::new(),
            queue_pos: None,
            queue_dir: None,
            tracks_dir: None,
            tracks_loading: false,
            scan: Worker::spawn("scan", wake.clone(), Listing::run),
            follow: Worker::spawn("follow", wake.clone(), Listing::run),
            follow_generation: 0,
            following_into: None,
            scan_generation: 0,
            memo: HashMap::new(),
            memo_order: VecDeque::new(),
            cover: None,
            picker,
            audio: AudioPlayer::new(wake.clone())?,
            shuffle: false,
            shuffle_order: Vec::new(),
            rng: Rng::new(),
            settings_file: config::settings_path(),
            remote: config::Remote::default(),
            transfer: None,
            wake: wake.clone(),
            settings_dirty: None,
            saved_position: Duration::ZERO,
            resume: None,
            resume_at: None,
            resume_playing: false,
            cover_size: None,
            cover_pending: false,
            cover_file: None,
            cover_memo: None,
            cover_loader: CoverLoader::new(wake.clone()),
            cover_generation: 0,
            cover_requested_box: (0, 0),
            art_budget: Rect::ZERO,
            play_area: Rect::ZERO,
            prev_area: Rect::ZERO,
            next_area: Rect::ZERO,
            seek_bar: Rect::ZERO,
            shuffle_area: Rect::ZERO,
            folder_rows: Rect::ZERO,
            track_rows: Rect::ZERO,
            tab_areas: [Rect::ZERO; Tab::ALL.len()],
            tab: Tab::Local,
            feeds: config::load_feeds(),
            feed_state: TableState::default().with_selected(Some(0)),
            feeds_file: config::feeds_path(),
            open: Worker::spawn("open", wake.clone(), |url: String, _: &Cancel| {
                let opened = player::open_url(&url).map_err(|err| format!("{err:#}"));
                (url, opened)
            }),
            open_generation: 0,
            opening: None,
            // One request and one parse: nothing worth interrupting halfway.
            fetch: Worker::spawn("feeds", wake.clone(), |url: String, _: &Cancel| {
                let bytes = crate::net::get(&url)?;
                crate::feed::parse(&String::from_utf8_lossy(&bytes))
            }),
            fetch_generation: 0,
            fetched_url: None,
            feed_loading: false,
            add_area: Rect::ZERO,
            remove_area: Rect::ZERO,
            feed_rows: Rect::ZERO,
            prompt: None,
            last_click: None,
            media,
            published: None,
            status: String::new(),
            should_quit: false,
            show_keys: false,
            star_areas: [Rect::ZERO; 3],
            track_star_x: None,
        };
        let empty = app.folders.is_empty()
            && library::remote_url(&app.root).is_none()
            && library::scan_tracks(&app.root).is_empty();
        app.status = if let Some(err) = unreachable {
            format!("error: {err}")
        } else if empty {
            format!("no audio found under {}", app.root.display())
        } else {
            format!("{} folders", app.folders.len())
        };

        app.reload_tracks();
        Ok(app)
    }

    /// Put the app back where the last session left it.
    ///
    /// A step the caller takes rather than something `new` does for itself, so that
    /// building an App never depends on what is in the user's config directory —
    /// which is what the tests need, and is honest besides: restoring is a choice.
    ///
    /// Everything here is a hint from a file that may be older than the library it
    /// describes: a folder can be deleted, a feed removed, a track renamed. So each
    /// step checks, and anything that no longer holds is dropped rather than
    /// reported — a first run and a stale line should both just start normally.
    pub fn restore(&mut self, settings: config::Settings) {
        self.remote = settings.remote.clone();
        if let Some(server) = self.remote.server.clone() {
            // Registers the token, so every address on that server finds it.
            match remote::Server::connect(&server, self.remote.token.as_deref()) {
                Ok(_) => self.relist_root(),
                Err(err) => self.status = format!("server: {err}"),
            }
        }
        self.audio.set_volume(settings.volume.get());
        self.shuffle = settings.shuffle;

        let mut session = settings.session;
        // What was local last time may have been moved to the server since, with
        // `u`: then it is found there, under the same path.
        session.folder = session.folder.map(|path| self.on_server_if_moved(path));
        session.selected = session.selected.map(|path| self.on_server_if_moved(path));
        session.queue = session.queue.map(|path| self.on_server_if_moved(path));
        session.track = session.track.map(|track| {
            self.on_server_if_moved(PathBuf::from(&track))
                .to_string_lossy()
                .into_owned()
        });
        // Back to what was playing, not to wherever the cursor had wandered: the
        // queue's folder, highlighted in the folder that holds it. Tracks lying
        // loose in the root have no row of their own, so there the cursor goes
        // back where it was.
        if session.track.is_some()
            && let Some(queue) = session.queue.take()
            && queue != self.root
            && let Some(parent) = queue.parent()
        {
            session.folder = Some(parent.to_path_buf());
            session.selected = Some(queue);
        }
        if let Some(folder) = session.folder.as_deref() {
            self.restore_folder(folder, session.selected.as_deref());
        }
        if let Some(url) = session.feed.as_deref()
            && let Some(index) = self.feeds.iter().position(|feed| feed.url == url)
        {
            self.feed_state.select(Some(index));
        }
        if let Some(tab) = session.tab.as_deref().and_then(Tab::from_key) {
            self.tab = tab;
        }
        // Held until a listing turns up that contains it; see `try_resume`.
        self.resume = session.track.map(|track| Resume {
            track,
            position: session.position,
            playing: session.playing,
        });

        // Whichever tab we landed on fills its own list — and only that one, or a
        // listing from the tab we are *not* on would be the first to arrive and
        // would spend the resume on itself.
        match self.tab {
            Tab::Local => self.reload_tracks(),
            Tab::Feeds => self.reload_feed(),
            Tab::Radio => {}
        }
        // Reloading is skipped when the listing wanted is the one already on
        // screen, and Radio has no listing at all. Both leave nothing that would
        // call `show_tracks`, and a resume that is never spent is one that goes
        // off later, when browsing happens to walk past its track.
        if !self.tracks_loading && !self.feed_loading {
            self.try_resume();
        }
    }

    /// Where a local path went if it was moved to the server: the same place under
    /// the server's address. Unchanged if it still exists here, is not in this
    /// library, or there is no server.
    fn on_server_if_moved(&self, path: PathBuf) -> PathBuf {
        let Some(server) = self.remote.server.as_deref() else {
            return path;
        };
        if path.exists() || library::remote_url(&path).is_some() {
            return path;
        }
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return path;
        };
        let rel = rel
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel.is_empty() {
            return path;
        }
        PathBuf::from(format!("{server}/{rel}"))
    }

    /// Walk back down to `folder`, rebuilding the trail as if it had been browsed
    /// to, so that Backspace still climbs out of it one level at a time.
    ///
    /// `selected` is the row that was highlighted there, which is what decides the
    /// track listing — often a subfolder, and the folder itself when it has none.
    fn restore_folder(&mut self, folder: &Path, selected: Option<&Path>) {
        // Only ever inside the root or on the server: the folder on the command
        // line decides what this run is browsing, and a remembered path from a
        // different library has no business overriding it.
        if !folder.starts_with(&self.root) && library::remote_url(folder).is_none() {
            return;
        }

        // Walk down the way browsing would have: at each level, into the row that
        // leads towards `folder`. A server's folders sit among the local ones at
        // the root, so this finds its way onto the server with no special case.
        let mut cwd = self.root.clone();
        let mut trail = Vec::new();
        while cwd != folder {
            let rows = self.list(&cwd).unwrap_or_default();
            let Some(row) = rows.iter().position(|sub| folder.starts_with(&sub.path)) else {
                // A folder that is no longer listed — deleted, or emptied of audio.
                // Stop at the deepest point that still exists.
                break;
            };
            // The row, not the index: every level carries a `..`.
            trail.push((cwd.clone(), row + 1));
            cwd = rows[row].path.clone();
        }

        self.cwd = cwd;
        self.trail = trail;
        self.folders = self.list(&self.cwd.clone()).unwrap_or_default();
        self.folder_state = TableState::default();

        // Land on the folder that was highlighted, and on the first real row when
        // it has gone.
        // `..` lists the folder itself, so that is the row that was on it.
        let row = if selected == Some(self.cwd.as_path()) {
            0
        } else {
            selected
                .and_then(|selected| self.folders.iter().position(|sub| sub.path == selected))
                .map(|index| index + 1)
                .unwrap_or(usize::from(!self.folders.is_empty()))
        };
        if self.folder_row_count() > 0 {
            self.folder_state
                .select(Some(row.min(self.folder_row_count() - 1)));
        }
    }

    /// Start the remembered track, if this listing is the one that holds it.
    ///
    /// One shot either way: whether or not it was found, the first listing to
    /// arrive spends it. Keeping it alive would mean that browsing into that
    /// folder an hour later would suddenly start playing on its own.
    fn try_resume(&mut self) {
        let Some(resume) = self.resume.take() else {
            return;
        };
        let Some(index) = self
            .tracks
            .iter()
            .position(|track| track.path.to_string_lossy() == resume.track)
        else {
            return;
        };
        let at = resume_point(&resume, self.tracks[index].duration);
        self.resume_at = Some((resume.track, at));
        self.resume_playing = resume.playing;
        self.play_index(index);
        self.track_state.select(Some(index));
    }

    /// Put the playhead where the restored track left off, and leave it playing or
    /// paused the way it was.
    ///
    /// Called once the source is actually queued, which for a local file is at once
    /// and for a stream is whenever the server gets round to it.
    ///
    /// It is held quiet either way until the seek has been answered, even when it
    /// is going to play: appending starts at the beginning of the track, and the
    /// few milliseconds before the seek lands would otherwise be a click of the
    /// wrong audio. rodio answers a seek from the same periodic callback whether
    /// it is paused or not, so the hold costs nothing. `poll_seek` lets it go.
    fn apply_resume_position(&mut self) {
        let Some((_, at)) = self.resume_at.take() else {
            return;
        };
        let playing = self.resume_playing;
        let title = self
            .now_playing()
            .map(|track| track.title.clone())
            .unwrap_or_default();

        if at > Duration::ZERO {
            self.audio.pause();
            self.audio.seek(at);
        } else {
            // Nothing to wait for at the start of a track.
            self.resume_playing = false;
            if !playing {
                self.audio.pause();
            }
        }

        self.status = match playing {
            true => format!("{title} — resumed at {}", library::fmt_duration(at)),
            false => format!("{title} — paused at {}", library::fmt_duration(at)),
        };
    }

    /// Note that the session has moved on and is owed a write.
    ///
    /// Deliberately not called as the playhead advances: the position is picked up
    /// by whatever write the rest of the session earns, and by the forced one on
    /// the way out. Marking it every second would be a file write every second.
    fn touch_settings(&mut self) {
        self.settings_dirty = Some(Instant::now());
    }

    /// True when row 0 of the folder pane is the `..` entry.
    ///
    /// It is a *row*, not a folder, so every mapping between the two goes through
    /// [`Self::folder_at`] and [`Self::folder_row_count`] rather than doing the
    /// off-by-one by hand.
    pub fn shows_up_row(&self) -> bool {
        // At the root too: there it climbs nowhere, and lists the whole library.
        true
    }

    /// What to ask a worker for to list `dir`. For `..` on a local folder that is
    /// every folder on screen — at the root, the server's among them — and what is
    /// still good of each. A server's own folder lists itself whole in one call.
    fn listing(&self, dir: PathBuf) -> Listing {
        if dir == self.cwd && library::remote_url(&self.cwd).is_none() {
            let parts = self
                .folders
                .iter()
                .map(|f| (f.path.clone(), f.stamp(), self.remembered(f)))
                .collect();
            return Listing {
                dir,
                stamp: None,
                parts,
            };
        }
        let stamp = self
            .folders
            .iter()
            .find(|f| f.path == dir)
            .map(Folder::stamp);
        Listing {
            dir,
            stamp,
            parts: Vec::new(),
        }
    }

    /// What was read of `folder`, unless it has changed since.
    fn remembered(&self, folder: &Folder) -> Option<Vec<Track>> {
        self.memo
            .get(&folder.path)
            .filter(|(stamp, _)| *stamp == folder.stamp())
            .map(|(_, tracks)| tracks.clone())
    }

    /// Rows in the folder pane, `..` included.
    pub fn folder_row_count(&self) -> usize {
        self.folders.len() + usize::from(self.shows_up_row()) + usize::from(self.shows_server_row())
    }

    /// True when the folder pane ends in the server button: `+ Add server`, or the
    /// server's name to change it. Only at the root of a local library, which is
    /// where the server's folders appear.
    pub fn shows_server_row(&self) -> bool {
        self.cwd == self.root && library::remote_url(&self.root).is_none()
    }

    /// True when the cursor is on the server button.
    pub fn on_server_row(&self) -> bool {
        self.shows_server_row() && self.folder_state.selected() == Some(self.folder_row_count() - 1)
    }

    /// The folder a row points at. `None` for the `..` row.
    pub fn folder_at(&self, row: usize) -> Option<&Folder> {
        let index = row.checked_sub(usize::from(self.shows_up_row()))?;
        self.folders.get(index)
    }

    /// True when the cursor is on `..`.
    pub fn on_up_row(&self) -> bool {
        self.shows_up_row() && self.folder_state.selected() == Some(0)
    }

    pub fn selected_folder(&self) -> Option<&Folder> {
        self.folder_at(self.folder_state.selected()?)
    }

    pub fn now_playing(&self) -> Option<&Track> {
        self.queue.get(self.queue_pos?)
    }

    /// Index in the *visible* list of the playing track, for the ▶ marker. Absent
    /// when you have browsed away from it.
    pub fn playing_row(&self) -> Option<usize> {
        let playing = &self.now_playing()?.path;
        self.tracks.iter().position(|t| &t.path == playing)
    }

    pub fn is_playing_something(&self) -> bool {
        self.queue_pos.is_some()
    }

    /// Ask for the tracks of whatever the cursor points at. Served from the memo
    /// when possible, otherwise handed to the worker: a deep scan reads tags, which
    /// is milliseconds per file and cannot sit on a cursor move.
    fn reload_tracks(&mut self) {
        let dir = self.listing_dir();
        if self.tracks_dir.as_ref() == Some(&dir) {
            return;
        }
        self.tracks_dir = Some(dir.clone());

        // `..` is put together from its folders each time instead, so a file added
        // loose beside them is never missed.
        let cached = self
            .folders
            .iter()
            .find(|f| f.path == dir)
            .and_then(|f| self.remembered(f));
        if let Some(tracks) = cached {
            // Whatever scan was running is for the folder we just left, and nothing
            // will clear the flag for it: this listing is already complete.
            self.tracks_loading = false;
            self.show_tracks(tracks);
            return;
        }
        self.tracks_loading = true;
        self.scan_generation += 1;
        let listing = self.listing(dir);
        self.scan.request(self.scan_generation, listing);
    }

    /// Block until the pending scan lands. Tests only: the real loop polls.
    #[cfg(test)]
    pub(crate) fn wait_for_tracks(&mut self) {
        for _ in 0..400 {
            self.poll_tracks();
            if !self.tracks_loading {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the scan never finished");
    }

    /// Pick up a finished scan. Cheap, so it can run every loop iteration.
    pub fn poll_tracks(&mut self) {
        let mut newest = None;
        for (generation, scanned) in self.scan.drain() {
            if generation == self.scan_generation {
                newest = Some(scanned);
            }
        }
        let Some(Listed {
            dir,
            stamp,
            tracks,
            found,
        }) = newest
        else {
            return;
        };
        for (path, stamp, listed) in found {
            self.remember(path, stamp, listed);
        }
        // The generation can still match a folder we have left: the memo path serves a
        // listing without asking for a scan, so it bumps nothing. Only the directory
        // says whose tracks these are.
        if self.tracks_dir.as_ref() != Some(&dir) {
            return;
        }
        self.tracks_loading = false;
        match tracks {
            Ok(tracks) => {
                if let Some(stamp) = stamp {
                    self.remember(dir, stamp, tracks.clone());
                }
                self.show_tracks(tracks);
            }
            // Not remembered, so moving back onto the folder asks again.
            Err(err) => {
                self.tracks_dir = None;
                self.status = format!("error: {err}");
                self.show_tracks(Vec::new());
            }
        }
    }

    fn show_tracks(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.track_state = TableState::default();
        if !self.tracks.is_empty() {
            self.track_state.select(Some(0));
        }
        self.try_resume();
    }

    /// Bounded so browsing a large tree cannot grow without limit.
    fn remember(&mut self, dir: PathBuf, stamp: Stamp, tracks: Vec<Track>) {
        const KEEP: usize = 256;
        if self.memo.insert(dir.clone(), (stamp, tracks)).is_none() {
            self.memo_order.push_back(dir);
        }
        while self.memo_order.len() > KEEP {
            if let Some(old) = self.memo_order.pop_front() {
                self.memo.remove(&old);
            }
        }
    }

    /// The directory whose tracks the right pane shows: the highlighted subfolder,
    /// or the current one when it has no subfolders of its own.
    /// On `..`, or with nothing selected, that is the current folder itself — which
    /// still lists something useful, namely everything below it.
    pub fn listing_dir(&self) -> PathBuf {
        self.selected_folder()
            .map(|f| f.path.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// What Enter does in the left pane, whichever tab is showing.
    pub fn enter_selected(&mut self) {
        match self.tab {
            // A feed has nothing to descend into; its episodes are already listed.
            Tab::Feeds => self.focus = Pane::Tracks,
            _ => self.enter_folder(),
        }
    }

    /// Descend into the highlighted folder, or back out when `..` is highlighted.
    pub fn enter_folder(&mut self) {
        if self.on_up_row() {
            // `..` at the root has nowhere to climb: what it lists is the point.
            if self.can_leave() {
                self.leave_folder();
            } else {
                self.focus = Pane::Tracks;
            }
            return;
        }
        if self.on_server_row() {
            self.open_add_server();
            return;
        }
        let Some(folder) = self.selected_folder() else {
            return;
        };
        let target = folder.path.clone();
        let subdirs = match self.list(&target) {
            Ok(subdirs) => subdirs,
            Err(err) => {
                self.status = format!("error: {err}");
                return;
            }
        };
        // A leaf album has nothing to descend into; the right pane already shows it.
        if subdirs.is_empty() {
            self.focus = Pane::Tracks;
            return;
        }
        // Remember the row, since `..` shifts them.
        self.trail
            .push((self.cwd.clone(), self.folder_state.selected().unwrap_or(0)));
        self.cwd = target;
        self.touch_settings();
        self.folders = subdirs;
        self.folder_state = TableState::default();
        // Land on the first real folder, not on `..`.
        self.folder_state
            .select(Some(usize::from(self.shows_up_row())));
        self.reload_tracks();
    }

    /// Step back up, restoring the row we came from. Stops at the root.
    pub fn leave_folder(&mut self) {
        let Some((parent, selected)) = self.trail.last().cloned() else {
            return;
        };
        let folders = match self.list(&parent) {
            Ok(folders) => folders,
            Err(err) => {
                self.status = format!("error: {err}");
                return;
            }
        };
        self.trail.pop();
        self.cwd = parent;
        self.touch_settings();
        self.folders = folders;
        self.folder_state = TableState::default();
        let rows = self.folder_row_count();
        if rows > 0 {
            self.folder_state.select(Some(selected.min(rows - 1)));
        }
        self.reload_tracks();
    }

    /// The folders of `dir`. At the root of a local library that is the local
    /// folders and the server's side by side, as one list: a folder that exists in
    /// both is shown once, as the local one, since that still has something to
    /// move.
    ///
    /// A server that cannot be reached leaves the local folders standing, and says
    /// why in the status line.
    fn list(&mut self, dir: &Path) -> Result<Vec<Folder>, String> {
        let mut folders = library::subdirs(dir)?;
        let Some(server) = self.remote.server.clone() else {
            return Ok(folders);
        };
        if dir != self.root || library::remote_url(&self.root).is_some() {
            return Ok(folders);
        }
        match library::subdirs(Path::new(&server)) {
            Ok(remote) => {
                let here: std::collections::HashSet<String> =
                    folders.iter().map(|f| f.label.clone()).collect();
                folders.extend(remote.into_iter().filter(|f| !here.contains(&f.label)));
                folders.sort_by(|a, b| a.label.cmp(&b.label));
            }
            Err(err) => self.status = format!("server: {err}"),
        }
        Ok(folders)
    }

    /// List the root again, keeping the cursor where it was as far as it can.
    fn relist_root(&mut self) {
        if self.cwd != self.root {
            return;
        }
        let root = self.root.clone();
        let on_up = self.on_up_row();
        let selected = self.selected_folder().map(|f| f.path.clone());
        self.folders = self.list(&root).unwrap_or_default();
        let row = if on_up {
            0
        } else {
            selected
                .and_then(|path| self.folders.iter().position(|f| f.path == path))
                .unwrap_or(0)
                + usize::from(!self.folders.is_empty())
        };
        self.folder_state = TableState::default().with_selected(Some(row));
        self.reload_tracks();
    }

    /// True for a local folder that `u` would move to the server.
    pub fn can_move(&self, folder: &Folder) -> bool {
        self.remote.server.is_some()
            && library::remote_url(&folder.path).is_none()
            && library::remote_url(&self.root).is_none()
            && folder.path.starts_with(&self.root)
    }

    pub fn open_add_server(&mut self) {
        let example = "e.g. nas:7700, or with its token: TOKEN@nas:7700";
        let hint = match self.remote.server.as_deref().and_then(remote::authority_of) {
            Some(current) => {
                format!("now {current}. Type a new address — {example} · Esc keeps it")
            }
            None => format!("server address — {example} · Enter adds · Esc cancels"),
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Server,
            title: "Add server",
            input: String::new(),
            hint,
            retry: None,
        });
    }

    fn submit_server(&mut self) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        if prompt.input.trim().is_empty() {
            self.prompt = None;
            return;
        }
        let (server, token) = match remote::split_address(&prompt.input) {
            Ok(split) => split,
            Err(err) => {
                prompt.hint = err;
                return;
            }
        };
        let connected = match remote::Server::connect(&server, token.as_deref()) {
            Ok(connected) => connected,
            Err(err) => {
                prompt.hint = err;
                return;
            }
        };
        // Try it now, while the address is still in the field to fix. A server
        // that is only switched off can still be added, on a second Enter.
        if prompt.retry.as_deref() != Some(prompt.input.as_str())
            && let Err(err) = connected.folders("")
        {
            prompt.hint = format!("{err} · Enter again to add it anyway");
            prompt.retry = Some(prompt.input.clone());
            return;
        }
        self.prompt = None;
        self.status = format!(
            "server {}",
            remote::authority_of(&server).unwrap_or_default()
        );
        self.remote = config::Remote {
            server: Some(server),
            token,
        };
        // Written now rather than on the usual delay: this is something the user
        // typed, not the playhead drifting.
        self.write_settings();
        self.memo.clear();
        self.memo_order.clear();
        // The root's listing now takes in a different server.
        self.tracks_dir = None;
        self.relist_root();
    }

    /// Move the highlighted local folder to the server, at once: `u`. The local
    /// copy is deleted file by file, as the server confirms each one.
    pub fn move_selected(&mut self) {
        if self.remote.server.is_none() {
            self.status = "no server yet: a adds one".into();
            return;
        }
        if self.is_moving() {
            self.status = "a move is already running".into();
            return;
        }
        let Some(folder) = self.selected_folder().cloned() else {
            return;
        };
        if !self.can_move(&folder) {
            self.status = format!("{} is already on the server", folder.label);
            return;
        }
        let Ok(rel) = folder.path.strip_prefix(&self.root) else {
            return;
        };
        let rel = rel
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        self.start_move(folder.path.clone(), rel);
    }

    /// Upload on a thread of its own, then delete what the server confirmed.
    fn start_move(&mut self, local: PathBuf, rel: String) {
        let Some(address) = self.remote.server.clone() else {
            return;
        };
        let server = match remote::Server::connect(&address, self.remote.token.as_deref()) {
            Ok(server) => server,
            Err(err) => {
                self.status = format!("error: {err}");
                return;
            }
        };
        let label = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let state = Arc::new(Mutex::new(Move {
            title: format!(
                "{label} → {}",
                remote::authority_of(&address).unwrap_or_default()
            ),
            base: rel.clone(),
            files: 0,
            bytes: 0,
            done_files: 0,
            skipped_bytes: 0,
            sending: Vec::new(),
            log: VecDeque::new(),
            started: Instant::now(),
            finished: None,
            failed: false,
        }));
        let uploaded = Arc::new(AtomicU64::new(0));
        self.transfer = Some(Transfer {
            state: Arc::clone(&state),
            uploaded: Arc::clone(&uploaded),
            visible: true,
            settled: false,
        });
        self.status = format!("moving {label}…");

        let wake = self.wake.clone();
        let root = self.root.clone();
        let spawned = std::thread::Builder::new()
            .name("move".into())
            .spawn(move || {
                let report = |step: remote::Step| {
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    match step {
                        remote::Step::Plan { files, bytes } => {
                            state.files = files;
                            state.bytes = bytes;
                        }
                        remote::Step::Start { remote, size, sent } => {
                            let name = state.name(remote);
                            state.sending.push((name, size, Arc::clone(sent)));
                        }
                        remote::Step::Failed { remote } => {
                            let name = state.name(remote);
                            state.sending.retain(|(n, _, _)| *n != name);
                        }
                        // Done files leave the card; the bar counts them.
                        remote::Step::Done { index, sent } => {
                            let name = state.name(&sent.remote);
                            state.done_files = index;
                            state.sending.retain(|(n, _, _)| *n != name);
                            if sent.skipped {
                                state.skipped_bytes += sent.size;
                            }
                        }
                    }
                    drop(state);
                    wake.nudge();
                };
                let pushed = remote::push(&server, &local, &rel, &uploaded, report);
                let removed = remote::remove_moved(&local, &pushed, &root);

                let Ok(mut state) = state.lock() else {
                    return;
                };
                state.sending.clear();
                if let Some(err) = &pushed.error {
                    state.note(LogLine::Failed(err.clone()));
                }
                let message = match (&pushed.error, removed) {
                    (None, Ok(n)) => {
                        state.note(LogLine::Note(format!("deleted {n} files here")));
                        format!("moved {label} to the server: {} files", pushed.done.len())
                    }
                    (Some(_), Ok(0)) => {
                        state.note(LogLine::Note("nothing deleted here".into()));
                        state.failed = true;
                        "move failed; nothing was deleted".to_string()
                    }
                    (Some(_), Ok(n)) => {
                        state.note(LogLine::Note(format!(
                            "deleted {n} files the server has; the rest stay here"
                        )));
                        state.failed = true;
                        format!("move stopped after {n} of {} files", pushed.total)
                    }
                    (_, Err(err)) => {
                        state.failed = true;
                        state.note(LogLine::Failed(format!("could not delete here: {err}")));
                        format!("uploaded, but could not delete here: {err}")
                    }
                };
                state.finished = Some(message);
                drop(state);
                wake.nudge();
            });
        if let Err(err) = spawned {
            self.transfer = None;
            self.status = format!("error: {err}");
        }
    }

    /// What `r`, `m` and `x` act on: the highlighted folder in the left pane, or
    /// the highlighted track in the right one. Its path, and what to call it.
    fn edit_target(&self) -> Option<(PathBuf, String)> {
        if self.tab != Tab::Local {
            return None;
        }
        match self.focus {
            Pane::Folders => self
                .selected_folder()
                .map(|folder| (folder.path.clone(), folder.label.clone())),
            Pane::Tracks => {
                let track = self.tracks.get(self.track_state.selected()?)?;
                let name = file_name(&track.path);
                Some((track.path.clone(), name))
            }
        }
    }

    /// The path of `target` from the top of its library: the local root, or the
    /// server's music folder.
    fn library_path(&self, target: &Path) -> Option<String> {
        match library::remote_url(target) {
            Some(url) => Some(remote::path_of(url)),
            None => Some(
                target
                    .strip_prefix(&self.root)
                    .ok()?
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
            ),
        }
    }

    /// `r`: a new name for the highlighted folder or track.
    pub fn ask_rename(&mut self) {
        let Some((target, name)) = self.edit_target() else {
            return;
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Rename(target),
            title: "Rename",
            input: name,
            hint: "new name · Enter renames · Esc cancels".into(),
            retry: None,
        });
    }

    /// `m`: a new place for the highlighted folder or track, as a path from the
    /// top of its library. Local things stay local and server things stay on the
    /// server; `u` is what crosses over.
    pub fn ask_relocate(&mut self) {
        let Some((target, _)) = self.edit_target() else {
            return;
        };
        let Some(path) = self.library_path(&target) else {
            return;
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Relocate(target),
            title: "Move to",
            input: path,
            hint: "path from the top of the library, folders are made as needed · Enter · Esc"
                .into(),
            retry: None,
        });
    }

    /// `x`: delete the highlighted folder or track, once `y` confirms it.
    pub fn ask_delete(&mut self) {
        let Some((target, name)) = self.edit_target() else {
            return;
        };
        let place = if library::remote_url(&target).is_some() {
            "on the server"
        } else {
            "here"
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Delete(target),
            title: "Delete",
            input: String::new(),
            hint: format!("delete {name} {place}, for good? y deletes · Esc keeps it"),
            retry: None,
        });
    }

    fn submit_rename(&mut self, target: PathBuf) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let name = prompt.input.trim().to_string();
        if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
            prompt.hint = "a name, without / — to put it elsewhere, use m".into();
            return;
        }
        let Some(path) = self.library_path(&target) else {
            return;
        };
        let to = match path.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{name}"),
            None => name.clone(),
        };
        self.change(&target, &to, format!("renamed to {name}"));
    }

    fn submit_relocate(&mut self, target: PathBuf) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let to = prompt.input.trim().trim_matches('/').to_string();
        if to.is_empty()
            || to
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            prompt.hint = "a path like Artist/Album, from the top of the library".into();
            return;
        }
        self.change(&target, &to, format!("moved to /{to}"));
    }

    /// Rename or move `target` to `to`, a path from the top of its own library.
    /// Refuses to overwrite: whatever is at `to` already stays.
    fn change(&mut self, target: &Path, to: &str, done: String) {
        let result = match library::remote_url(target) {
            Some(url) => remote::move_to(url, to),
            None => {
                let dest = to
                    .split('/')
                    .fold(self.root.clone(), |path, part| path.join(part));
                if dest.exists() {
                    Err(format!("/{to} already exists"))
                } else if dest.starts_with(target) {
                    Err("cannot move a folder into itself".into())
                } else {
                    dest.parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| std::fs::rename(target, &dest))
                        .map_err(|err| err.to_string())
                }
            }
        };
        match result {
            Ok(()) => {
                self.prompt = None;
                self.status = done;
                self.refresh_listing();
            }
            // Kept open, so a typo can be fixed rather than typed again.
            Err(err) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.hint = err;
                }
            }
        }
    }

    fn delete(&mut self, target: &Path) {
        let name = file_name(target);
        let result = match library::remote_url(target) {
            Some(url) => remote::remove(url),
            // Never the library itself — only ever something listed inside it.
            None if target == self.root || !target.starts_with(&self.root) => {
                Err("refusing to delete the library itself".into())
            }
            None if target.is_dir() => std::fs::remove_dir_all(target).map_err(|e| e.to_string()),
            None => std::fs::remove_file(target).map_err(|e| e.to_string()),
        };
        self.status = match result {
            Ok(()) => format!("deleted {name}"),
            Err(err) => format!("could not delete {name}: {err}"),
        };
        self.refresh_listing();
    }

    /// List the folder on screen again after something in it changed, keeping the
    /// cursor on the same row as far as the new list allows.
    fn refresh_listing(&mut self) {
        self.memo.clear();
        self.memo_order.clear();
        self.tracks_dir = None;
        let cwd = self.cwd.clone();
        let selected = self.folder_state.selected();
        self.folders = self.list(&cwd).unwrap_or_default();
        self.folder_state = TableState::default();
        let rows = self.folder_row_count();
        if rows > 0 {
            self.folder_state
                .select(Some(selected.unwrap_or(0).min(rows - 1)));
        }
        self.reload_tracks();
    }

    /// True while a folder is on its way to the server.
    pub fn is_moving(&self) -> bool {
        self.transfer
            .as_ref()
            .is_some_and(|transfer| !transfer.settled)
    }

    /// Show or hide the log of the last move. `l`.
    pub fn toggle_transfer_log(&mut self) {
        match self.transfer.as_mut() {
            Some(transfer) => transfer.visible = !transfer.visible,
            None => self.status = "nothing has been moved yet".into(),
        }
    }

    /// Hide the log if it is showing. True if it was, so Escape closes the log
    /// rather than the player.
    pub fn hide_transfer_log(&mut self) -> bool {
        match self.transfer.as_mut() {
            Some(transfer) if transfer.visible => {
                transfer.visible = false;
                true
            }
            _ => false,
        }
    }

    /// The log panel's contents, if it is showing.
    pub fn transfer_view(&self) -> Option<TransferView> {
        let transfer = self.transfer.as_ref().filter(|t| t.visible)?;
        let state = transfer.state.lock().ok()?;
        let uploaded = transfer.uploaded.load(Ordering::Relaxed);
        let elapsed = state.started.elapsed().as_secs_f64().max(0.001);
        let speed = uploaded as f64 / elapsed;
        let bytes_done = (uploaded + state.skipped_bytes).min(state.bytes);
        let left = (state.finished.is_none() && speed > 0.0).then(|| {
            Duration::from_secs_f64(state.bytes.saturating_sub(bytes_done) as f64 / speed)
        });
        Some(TransferView {
            title: state.title.clone(),
            log: state.log.iter().cloned().collect(),
            sending: state
                .sending
                .iter()
                .map(|(name, size, sent)| {
                    (name.clone(), sent.load(Ordering::Relaxed).min(*size), *size)
                })
                .collect(),
            files_done: state.done_files,
            files: state.files,
            bytes_done,
            bytes: state.bytes,
            speed,
            left,
            finished: state.finished.clone(),
            failed: state.failed,
        })
    }

    /// Once a move is done, list the library as it now is. The log stays up until
    /// it is hidden.
    pub fn poll_move(&mut self) {
        let Some(transfer) = self.transfer.as_ref() else {
            return;
        };
        if transfer.settled {
            return;
        }
        let finished = transfer
            .state
            .lock()
            .ok()
            .and_then(|state| state.finished.clone());
        let Some(message) = finished else {
            return;
        };
        if let Some(transfer) = self.transfer.as_mut() {
            transfer.settled = true;
        }
        self.status = message;
        // Both sides changed: nothing remembered about either still holds.
        self.refresh_listing();
    }

    /// Block until a move has finished. Tests only: the real loop polls.
    #[cfg(test)]
    pub(crate) fn wait_for_move(&mut self) {
        for _ in 0..2000 {
            self.poll_move();
            if !self.is_moving() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the move never finished");
    }

    pub fn can_leave(&self) -> bool {
        !self.trail.is_empty()
    }

    /// Breadcrumb for the pane title: the root's name plus the way down.
    pub fn here(&self) -> String {
        let root_name = match library::remote_url(&self.root) {
            Some(url) => remote::authority_of(url).unwrap_or_default(),
            None => self
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.root.display().to_string()),
        };
        // On the server, beside the local folders: the same breadcrumb, marked.
        if self.cwd != self.root
            && let Some(url) = library::remote_url(&self.cwd)
        {
            let path = remote::path_of(url).replace('/', " / ");
            return match library::remote_url(&self.root) {
                Some(_) => format!("{root_name} / {path}"),
                None => format!("{root_name} / {path} ☁"),
            };
        }
        match self.cwd.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => root_name,
            Ok(rest) => {
                let path = rest
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, " / ");
                format!("{root_name} / {path}")
            }
            Err(_) => self.cwd.display().to_string(),
        }
    }

    pub fn play_selected_track(&mut self) {
        let Some(idx) = self.track_state.selected() else {
            return;
        };
        self.play_index(idx);
    }

    /// Start `idx` of the visible list, and adopt that list as the queue so
    /// browsing elsewhere afterwards cannot derail next and previous.
    pub fn play_index(&mut self, idx: usize) {
        if self.tracks.get(idx).is_none() {
            return;
        }
        self.queue = self.tracks.clone();
        self.queue_dir = match self.tab {
            Tab::Local => self.tracks_dir.clone(),
            _ => None,
        };
        if self.shuffle {
            self.reshuffle_from(Some(idx));
        }
        self.play_queue_index(idx);
    }

    fn play_queue_index(&mut self, idx: usize) {
        let Some(track) = self.queue.get(idx) else {
            return;
        };
        let path = track.path.clone();
        // A restored position belongs to one track. Starting any other one means
        // the session has moved on — while a stream was opening, most likely —
        // and the playhead must not be dragged into wherever it lands next.
        if self
            .resume_at
            .as_ref()
            .is_some_and(|(wanted, _)| wanted.as_str() != path.to_string_lossy())
        {
            self.resume_at = None;
            self.resume_playing = false;
        }
        let art_url = track.art_url.clone();
        let title = track.title.clone();

        // A remote track streams; the identity in `path` is its URL either way.
        let Some(url) = track.url.clone() else {
            match self.audio.play_file(&path) {
                Ok(()) => {
                    self.opening = None;
                    self.queue_pos = Some(idx);
                    self.status = format!("playing {title}");
                    self.request_cover(&path, art_url);
                    self.apply_resume_position();
                    self.touch_settings();
                }
                Err(err) => self.status = format!("error: {err:#}"),
            }
            return;
        };

        // Opening a stream blocks for as long as the server takes, so it goes to a
        // worker and the answer is picked up in `poll_open`. The old track stops
        // now rather than playing on under the new title.
        self.audio.stop();
        self.queue_pos = Some(idx);
        self.opening = Some(url.clone());
        self.open_generation += 1;
        self.open.request(self.open_generation, url);
        self.status = format!("opening {title}");
        self.request_cover(&path, art_url);
        self.touch_settings();
    }

    /// True while a stream is being opened, so nothing takes the silence for the
    /// end of a track.
    pub fn is_opening(&self) -> bool {
        self.opening.is_some()
    }

    /// Pick up a stream that finished opening. Cheap, so it runs every loop.
    pub fn poll_open(&mut self) {
        let mut newest = None;
        for (generation, opened) in self.open.drain() {
            if generation == self.open_generation {
                newest = Some(opened);
            }
        }
        let Some((url, opened)) = newest else {
            return;
        };
        // Same rule as the scan: the generation alone cannot say whether this is
        // still the track we are waiting for. Only the URL can.
        if self.opening.as_deref() != Some(url.as_str()) {
            return;
        }
        self.opening = None;

        match opened {
            Ok(source) => {
                self.audio.play_remote(source);
                let title = self
                    .now_playing()
                    .map(|track| track.title.clone())
                    .unwrap_or_default();
                self.status = format!("playing {title}");
                self.apply_resume_position();
            }
            Err(err) => {
                self.queue_pos = None;
                self.resume_at = None;
                self.resume_playing = false;
                self.status = format!("error: {err}");
            }
        }
    }

    /// Hand the cover off to the worker and carry on. The old cover is cleared at
    /// once so a stale image never sits under a new track's title.
    fn request_cover(&mut self, path: &Path, art_url: Option<String>) {
        let box_px = self.art_box_px();

        // Same album, same pane: show the picture we already hold. The worker still
        // runs and will replace it, so a folder with per-track art self-corrects
        // instead of being stuck on the wrong cover.
        let reused = match (&self.cover_memo, path.parent()) {
            (Some((dir, memo_box, img)), Some(parent)) if dir == parent && *memo_box == box_px => {
                self.cover_size = Some((img.width().max(1), img.height().max(1)));
                self.cover = Some(self.picker.new_resize_protocol(img.clone()));
                true
            }
            _ => false,
        };
        if !reused {
            self.cover = None;
            self.cover_size = None;
            self.cover_file = None;
        }

        self.cover_pending = true;
        self.cover_generation += 1;
        self.cover_requested_box = box_px;
        self.cover_loader.request(cover::Request {
            generation: self.cover_generation,
            path: path.to_path_buf(),
            art_url,
            box_px: self.cover_requested_box,
        });
    }

    /// Re-scale the cover after the pane changed size. The worker pre-scales to
    /// exactly fit the pane, so any real change needs a new pass — otherwise the
    /// render thread would have to resize, which is what the worker exists to avoid.
    ///
    /// A small threshold keeps a one-column nudge from restarting the work.
    pub fn refresh_cover_for_resize(&mut self) {
        const SLACK: u32 = 16;
        if self.cover_pending || self.cover_requested_box == (0, 0) {
            return;
        }
        let (want_w, want_h) = self.art_box_px();
        let (had_w, had_h) = self.cover_requested_box;
        if want_w.abs_diff(had_w) < SLACK && want_h.abs_diff(had_h) < SLACK {
            return;
        }
        let Some((path, art_url)) = self
            .now_playing()
            .map(|t| (t.path.clone(), t.art_url.clone()))
        else {
            return;
        };
        self.request_cover(&path, art_url);
    }

    /// The pixel box the art will occupy.
    ///
    /// Must be exact: the worker scales to fill it, and the renderer then draws the
    /// result 1:1. Asking for the wrong size puts a resize back on the render thread.
    fn art_box_px(&self) -> (u32, u32) {
        /// Used only before the first frame, when the budget is still zero.
        const FALLBACK: (u32, u32) = (512, 512);
        let font = self.picker.font_size();
        let width = self.art_budget.width as u32 * font.width.max(1) as u32;
        let height = self.art_budget.height as u32 * font.height.max(1) as u32;
        if width == 0 || height == 0 {
            FALLBACK
        } else {
            (width, height)
        }
    }

    /// Pick up finished covers. Cheap, so it can run every loop iteration.
    pub fn poll_cover(&mut self) {
        // Keep only the newest reply; anything older is already superseded.
        let mut newest = None;
        for loaded in self.cover_loader.drain() {
            if loaded.generation == self.cover_generation {
                newest = Some(loaded);
            }
        }
        let Some(loaded) = newest else {
            return;
        };

        self.cover_pending = false;
        self.cover_file = loaded.file;
        match loaded.image {
            Some(img) => {
                self.cover_size = Some((img.width().max(1), img.height().max(1)));
                // Keep a copy so the rest of the album needs no worker at all.
                if let Some(dir) = self
                    .now_playing()
                    .and_then(|t| t.path.parent())
                    .map(Path::to_path_buf)
                {
                    self.cover_memo = Some((dir, self.cover_requested_box, img.clone()));
                }
                self.cover = Some(self.picker.new_resize_protocol(img));
            }
            None => {
                self.cover_size = None;
                self.cover = None;
                self.cover_memo = None;
            }
        }
    }

    pub fn toggle_play(&mut self) {
        if self.queue_pos.is_none() {
            self.play_selected_track();
        } else {
            self.audio.toggle();
        }
    }

    /// Stop, and let go of a stream still opening: it would otherwise land later
    /// and start playing on its own.
    fn stop_playback(&mut self) {
        self.audio.stop();
        self.opening = None;
        self.following_into = None;
        self.queue_pos = None;
        self.resume_at = None;
        self.resume_playing = false;
    }

    pub fn next_track(&mut self) {
        match self.queue_pos {
            Some(current) => match self.following(current) {
                Some(next) => self.play_queue_index(next),
                None => self.follow_on(),
            },
            // Nothing queued yet: start from whatever is on screen.
            None => self.play_selected_track(),
        }
    }

    /// The queue is used up: go on into the folder after the one it came from, the
    /// way the folder pane lists them, climbing out when that was the last one.
    /// Stops only at the end of the library, or of a feed.
    fn follow_on(&mut self) {
        let next = self
            .queue_dir
            .clone()
            .and_then(|dir| self.folder_after(&dir));
        let Some(next) = next else {
            self.stop_playback();
            self.status = "end of queue".into();
            return;
        };
        self.audio.stop();
        self.status = format!("on to {}", next.label);
        self.follow_into(next);
    }

    fn follow_into(&mut self, folder: Folder) {
        if let Some(tracks) = self.remembered(&folder) {
            return self.play_folder(folder.path, tracks);
        }
        self.following_into = Some(folder.path.clone());
        self.follow_generation += 1;
        let listing = Listing {
            stamp: Some(folder.stamp()),
            dir: folder.path,
            parts: Vec::new(),
        };
        self.follow.request(self.follow_generation, listing);
    }

    /// Pick up the folder playback is running on into. Cheap, so it runs every loop.
    pub fn poll_follow(&mut self) {
        let mut newest = None;
        for (generation, listed) in self.follow.drain() {
            if generation == self.follow_generation {
                newest = Some(listed);
            }
        }
        let Some(Listed {
            dir,
            stamp,
            tracks: listed,
            found,
        }) = newest
        else {
            return;
        };
        for (path, stamp, tracks) in found {
            self.remember(path, stamp, tracks);
        }
        // Stopped, or something else started, while it was being listed.
        if self.following_into.as_ref() != Some(&dir) {
            return;
        }
        self.following_into = None;
        match listed {
            Ok(tracks) => {
                if let Some(stamp) = stamp {
                    self.remember(dir.clone(), stamp, tracks.clone());
                }
                self.play_folder(dir, tracks);
            }
            Err(err) => {
                self.stop_playback();
                self.status = format!("error: {err}");
            }
        }
    }

    /// Make `dir` the queue and start it from the top — or from wherever shuffle
    /// says — moving the cursor along when it was on the folder just finished.
    fn play_folder(&mut self, dir: PathBuf, tracks: Vec<Track>) {
        let finished = self.queue_dir.replace(dir.clone());
        self.queue = tracks;
        if self.queue.is_empty() {
            // Nothing playable after all; the count said otherwise. Keep going.
            return self.follow_on();
        }
        let first = if self.shuffle {
            self.reshuffle_from(None);
            self.shuffle_order[0]
        } else {
            0
        };
        if self.tab == Tab::Local
            && finished.is_some()
            && self.selected_folder().map(|f| &f.path) == finished.as_ref()
            && let Some(index) = self.folders.iter().position(|f| f.path == dir)
        {
            self.folder_state.select(Some(index + 1));
            self.reload_tracks();
        }
        self.play_queue_index(first);
    }

    /// The folder listed after `dir` in its parent, or after its parent in the
    /// grandparent, and so on up to the root. `None` past the last one.
    fn folder_after(&mut self, dir: &Path) -> Option<Folder> {
        let mut dir = dir.to_path_buf();
        while dir != self.root {
            let parent = self.parent_of(&dir)?;
            let siblings = self.list(&parent).ok()?;
            let at = siblings.iter().position(|f| f.path == dir)?;
            if let Some(next) = siblings.get(at + 1) {
                return Some(next.clone());
            }
            dir = parent;
        }
        None
    }

    /// The folder `dir` is listed in. A server's top-level folders sit among the
    /// local ones, so their parent is the local root, not the server's own.
    fn parent_of(&self, dir: &Path) -> Option<PathBuf> {
        if library::remote_url(&self.root).is_none()
            && let Some(server) = self.remote.server.as_deref()
            && let Some(url) = library::remote_url(dir)
            && url
                .strip_prefix(server)
                .is_some_and(|rest| !rest.trim_matches('/').contains('/'))
        {
            return Some(self.root.clone());
        }
        let parent = dir.parent()?;
        (parent.starts_with(&self.root) || library::remote_url(parent).is_some())
            .then(|| parent.to_path_buf())
    }

    pub fn prev_track(&mut self) {
        if let Some(current) = self.queue_pos
            && let Some(previous) = self.preceding(current)
        {
            self.play_queue_index(previous);
        }
    }

    /// Give `track` this many stars — or take them away, when it already has
    /// exactly that many, the way a second click on the same star undoes it.
    ///
    /// Only a track on a server has stars: the server keeps them, which is what
    /// lets every player browsing it see the same ones.
    pub fn rate(&mut self, track: &Track, stars: u8) {
        let Some(url) = library::remote_url(&track.path).map(str::to_string) else {
            self.status = "stars are for tracks on the server".into();
            return;
        };
        let stars = stars.clamp(1, 3);
        let stars = if track.stars == Some(stars) {
            None
        } else {
            Some(stars)
        };
        if let Err(err) = remote::set_rating(&url, stars.unwrap_or(0)) {
            self.status = format!("stars: {err}");
            return;
        }
        // Every copy of the track this player holds, so the list, the queue and
        // the memo all agree without asking the server again.
        let copies = self.tracks.iter_mut().chain(self.queue.iter_mut()).chain(
            self.memo
                .values_mut()
                .flat_map(|(_, tracks)| tracks.iter_mut()),
        );
        for copy in copies.filter(|copy| copy.path == track.path) {
            copy.stars = stars;
        }
        self.status = match stars {
            Some(_) => format!("{} {}", stars_text(stars), track.title),
            None => format!("no stars for {}", track.title),
        };
    }

    /// `*`: one star more for what is playing, round to none after three.
    pub fn cycle_rating(&mut self) {
        let Some(track) = self.now_playing().cloned() else {
            return;
        };
        match track.stars {
            Some(3) => self.rate(&track, 3),
            Some(stars) => self.rate(&track, stars + 1),
            None => self.rate(&track, 1),
        }
    }

    /// Turn shuffling on or off.
    ///
    /// Turning it on scrambles the rest of the queue but leaves the current track
    /// playing — it is the order that changes, not what you are listening to.
    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.touch_settings();
        if self.shuffle {
            self.reshuffle_from(self.queue_pos);
            self.status = "shuffle on".into();
        } else {
            self.shuffle_order.clear();
            self.status = "shuffle off".into();
        }
    }

    /// A fresh permutation of the queue, `head` first when it is given.
    fn reshuffle_from(&mut self, head: Option<usize>) {
        let mut order: Vec<usize> = (0..self.queue.len()).collect();
        // Fisher-Yates, back to front.
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i + 1));
        }
        // Swapping rather than removing and re-inserting: the displaced entry
        // takes the head's old slot, which is as good a place as any.
        if let Some(head) = head
            && let Some(at) = order.iter().position(|&i| i == head)
        {
            order.swap(0, at);
        }
        self.shuffle_order = order;
    }

    /// What follows `current` in the queue, or `None` at the end of it.
    fn following(&self, current: usize) -> Option<usize> {
        if !self.shuffle {
            return (current + 1 < self.queue.len()).then_some(current + 1);
        }
        let at = self.shuffle_order.iter().position(|&i| i == current)?;
        self.shuffle_order.get(at + 1).copied()
    }

    /// What came before `current`, or `None` at the start.
    fn preceding(&self, current: usize) -> Option<usize> {
        if !self.shuffle {
            return current.checked_sub(1);
        }
        let at = self.shuffle_order.iter().position(|&i| i == current)?;
        self.shuffle_order.get(at.checked_sub(1)?).copied()
    }

    /// Move the volume and remember it. The write is deferred: a held-down `+`
    /// would otherwise put a file write behind every key repeat.
    pub fn nudge_volume(&mut self, delta: rodio::Float) {
        self.audio.nudge_volume(delta);
        self.touch_settings();
    }

    /// Write the settings out if they are owed and have settled.
    ///
    /// `force` skips the wait, for the way out: whatever the last keystroke was
    /// must survive quitting even if it was a moment ago — and so must the
    /// playhead, which never announces itself at all.
    pub fn save_settings(&mut self, force: bool) {
        /// Long enough that a key repeat writes once at the end of the burst.
        const SETTLE: Duration = Duration::from_millis(400);

        if !force {
            let Some(changed) = self.settings_dirty else {
                return;
            };
            if changed.elapsed() < SETTLE {
                return;
            }
        }
        self.write_settings();
    }

    /// Gather the session and put it on disk. Unconditional; the callers decide.
    fn write_settings(&mut self) {
        self.settings_dirty = None;
        let Some(path) = self.settings_file.clone() else {
            return;
        };
        let settings = config::Settings {
            volume: config::Volume::new(self.audio.volume()),
            shuffle: self.shuffle,
            session: config::Session {
                tab: Some(self.tab.key().to_string()),
                folder: Some(self.cwd.clone()),
                selected: Some(self.listing_dir()),
                feed: self.selected_feed().map(|feed| feed.url.clone()),
                track: self
                    .now_playing()
                    .map(|track| track.path.to_string_lossy().into_owned()),
                position: self.audio.position(),
                playing: self.is_playing_something() && !self.audio.is_paused(),
                queue: self.now_playing().and(self.queue_dir.clone()),
            },
            remote: self.remote.clone(),
        };
        self.saved_position = settings.session.position;
        if let Err(err) = config::save_settings_to(&path, &settings) {
            self.status = format!("could not save settings: {err}");
        }
    }

    /// Pick up the answer to a seek. Seeks are answered on their own thread now, so
    /// both the reason one failed and the fact one landed arrive after the event.
    pub fn poll_seek(&mut self) {
        let Some(result) = self.audio.seek_result() else {
            return;
        };
        if let Err(err) = result {
            self.status = format!("seek failed: {err}");
        }
        // A restored track was held quiet until its playhead was in the right
        // place. Released on a failed seek too: the alternative is leaving it
        // paused forever because a decoder would not seek.
        if std::mem::take(&mut self.resume_playing) {
            self.audio.play();
        }
    }

    /// Act on media keys, headphone buttons and Control Center / MPRIS.
    pub fn poll_media(&mut self) {
        let commands: Vec<Command> = self.media.commands().collect();
        for command in commands {
            match command {
                Command::Toggle => self.toggle_play(),
                Command::Play if self.queue_pos.is_none() => self.play_selected_track(),
                Command::Play => {
                    if self.audio.is_paused() {
                        self.audio.toggle();
                    }
                }
                Command::Pause => {
                    if !self.audio.is_paused() {
                        self.audio.toggle();
                    }
                }
                Command::Stop => self.stop_playback(),
                Command::Next => self.next_track(),
                Command::Previous => self.prev_track(),
                Command::SeekBy(delta) => self.seek_by(delta),
                Command::SetPosition(at) => {
                    if let Some(total) = self.now_playing().and_then(|t| t.duration) {
                        let fraction = at.as_secs_f32() / total.as_secs_f32().max(f32::EPSILON);
                        self.seek_to(fraction);
                    }
                }
            }
        }
    }

    /// Tell the OS what is playing. Cheap to call often: the host drops updates
    /// that match what it already published.
    pub fn publish_now_playing(&mut self) {
        let now = match self.now_playing() {
            Some(track) => NowPlaying {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration: track.duration,
                // Round off, or a once-a-second publish would never match.
                elapsed: Duration::from_secs(self.audio.position().as_secs()),
                playing: !self.audio.is_paused(),
                cover: self.cover_file.clone(),
            },
            None => NowPlaying::default(),
        };
        if self.published.as_ref() == Some(&now) {
            return;
        }
        self.media.publish(now.clone());
        self.published = Some(now);
    }

    /// Advance automatically when the current source has drained.
    ///
    /// A stream still opening has nothing queued either, and that silence must not
    /// read as a track that finished — it would skip the whole queue in a blur.
    pub fn tick(&mut self) {
        if self.queue_pos.is_some()
            && !self.is_opening()
            && self.following_into.is_none()
            && self.audio.is_finished()
            && !self.audio.is_paused()
        {
            self.next_track();
        }

        // The playhead is the only part of the session that moves without anyone
        // asking, so it is the only part that has to be noticed rather than
        // announced. Once a second: the file is a couple of hundred bytes, which
        // is less work than one of the frames already drawn every 120ms, and it
        // means a kill, a crash or a closed terminal costs a second rather than
        // the whole sitting.
        //
        // Written straight out rather than marked: the settle delay is there to
        // batch a key repeat, and a threshold of a second is already its own
        // batching. Going through it as well would only make the cadence 1.4s.
        const SAVE_POSITION_EVERY: Duration = Duration::from_secs(1);
        if self.is_playing_something()
            && !self.audio.is_paused()
            && self.audio.position().abs_diff(self.saved_position) >= SAVE_POSITION_EVERY
        {
            self.write_settings();
        }

        self.save_settings(false);
    }

    /// Move within the focused pane.
    ///
    /// `Pane::Folders` is "the left pane", whatever the tab puts there — folders on
    /// Local, feeds on Feeds. Routing keys by tab instead of by focus was what left
    /// the episode list unreachable.
    pub fn move_selection(&mut self, delta: isize) {
        if self.tab == Tab::Feeds && self.focus == Pane::Folders {
            self.move_feed_selection(delta);
            return;
        }

        let folder_rows = self.folder_row_count();
        let (len, state) = match self.focus {
            Pane::Folders => (folder_rows, &mut self.folder_state),
            Pane::Tracks => (self.tracks.len(), &mut self.track_state),
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        state.select(Some(next));

        if self.focus == Pane::Folders {
            self.reload_tracks();
        }
    }

    /// Switching sources must never interrupt playback: the queue is a snapshot, so
    /// what is playing outlives whatever the panes are showing.
    pub fn select_tab(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.touch_settings();
        // Each tab owns the track list, so entering one has to refill it.
        match tab {
            Tab::Local => {
                self.tracks_dir = None;
                self.reload_tracks();
            }
            Tab::Feeds => {
                self.fetched_url = None;
                self.reload_feed();
            }
            Tab::Radio => {}
        }
    }

    pub fn selected_feed(&self) -> Option<&Feed> {
        self.feeds.get(self.feed_state.selected()?)
    }

    /// Fetch the highlighted feed's episodes, unless they are already on screen.
    pub fn reload_feed(&mut self) {
        let Some(url) = self.selected_feed().map(|f| f.url.clone()) else {
            self.tracks.clear();
            self.fetched_url = None;
            return;
        };
        if self.fetched_url.as_ref() == Some(&url) {
            return;
        }
        self.fetched_url = Some(url.clone());
        self.touch_settings();
        self.tracks.clear();
        self.feed_loading = true;
        self.fetch_generation += 1;
        self.fetch.request(self.fetch_generation, url);
    }

    /// Pick up a finished fetch. Cheap, so it runs every loop iteration.
    pub fn poll_feed(&mut self) {
        let mut newest = None;
        for (generation, result) in self.fetch.drain() {
            if generation == self.fetch_generation {
                newest = Some(result);
            }
        }
        let Some(result) = newest else {
            return;
        };
        self.feed_loading = false;
        match result {
            Ok(channel) => {
                let tracks = library::tracks_from_feed(&channel);
                self.status = format!("{}: {} episodes", channel.title, tracks.len());
                self.adopt_channel_title(&channel.title);
                self.show_tracks(tracks);
            }
            Err(err) => {
                self.status = format!("feed failed: {err}");
                self.show_tracks(Vec::new());
            }
        }
    }

    /// Name a feed after its channel, now that we know it.
    ///
    /// Only when the current name was derived from the host — a name the user typed
    /// into `feeds.txt` is theirs to keep, which is what `name = url` is for.
    fn adopt_channel_title(&mut self, title: &str) {
        if title.is_empty() {
            return;
        }
        let Some(index) = self.feed_state.selected() else {
            return;
        };
        let Some(feed) = self.feeds.get_mut(index) else {
            return;
        };
        if feed.name != config::host_of(&feed.url) || feed.name == title {
            return;
        }
        feed.name = title.to_string();
        self.persist_feeds();
    }

    /// Move within the feed list, refetching as the cursor lands.
    pub fn move_feed_selection(&mut self, delta: isize) {
        if self.feeds.is_empty() {
            return;
        }
        let current = self.feed_state.selected().unwrap_or(0) as isize;
        let last = self.feeds.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        if self.feed_state.selected() != Some(next) {
            self.feed_state.select(Some(next));
            self.reload_feed();
        }
    }

    pub fn open_add_feed(&mut self) {
        self.prompt = Some(Prompt {
            kind: PromptKind::Feed,
            title: "Add feed",
            input: String::new(),
            hint: "paste an RSS URL · Enter to add · Esc to cancel".into(),
            retry: None,
        });
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    /// Feed a keystroke to the open input. Returns whether it was consumed, so the
    /// caller knows not to also treat it as a shortcut.
    pub fn prompt_key(&mut self, key: char) -> bool {
        // Deleting asks one question, answered with one key: nothing typed here
        // is text, and nothing but `y` goes ahead.
        if let Some(Prompt {
            kind: PromptKind::Delete(target),
            ..
        }) = &self.prompt
        {
            if matches!(key, 'y' | 'Y') {
                let target = target.clone();
                self.prompt = None;
                self.delete(&target);
            }
            return true;
        }
        match self.prompt.as_mut() {
            Some(prompt) => {
                prompt.input.push(key);
                true
            }
            None => false,
        }
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.input.pop();
        }
    }

    /// Accept the typed URL. Stays open with a reason when it is not usable, since
    /// closing on a typo would throw away what was pasted.
    pub fn submit_prompt(&mut self) {
        match self.prompt.as_ref().map(|prompt| prompt.kind.clone()) {
            Some(PromptKind::Server) => return self.submit_server(),
            Some(PromptKind::Rename(target)) => return self.submit_rename(target),
            Some(PromptKind::Relocate(target)) => return self.submit_relocate(target),
            // Only `y` deletes; Enter is not an answer.
            Some(PromptKind::Delete(_)) => return,
            Some(PromptKind::Feed) => {}
            None => return,
        }
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let url = prompt.input.trim().to_string();
        if url.is_empty() {
            self.prompt = None;
            return;
        }
        if !config::is_url(&url) {
            prompt.hint = "needs to start with http:// or https://".into();
            return;
        }
        if self.feeds.iter().any(|feed| feed.url == url) {
            prompt.hint = "already in the list".into();
            return;
        }

        self.feeds.push(Feed {
            name: config::host_of(&url),
            url,
        });
        self.feed_state.select(Some(self.feeds.len() - 1));
        self.prompt = None;
        self.persist_feeds();
        self.reload_feed();
    }

    /// Drop the highlighted feed.
    pub fn remove_selected_feed(&mut self) {
        let Some(index) = self.feed_state.selected() else {
            return;
        };
        if index >= self.feeds.len() {
            return;
        }
        let gone = self.feeds.remove(index);
        if self.feeds.is_empty() {
            self.feed_state.select(None);
        } else {
            self.feed_state
                .select(Some(index.min(self.feeds.len() - 1)));
        }
        self.status = format!("removed {}", gone.name);
        self.persist_feeds();
        self.reload_feed();
    }

    fn persist_feeds(&mut self) {
        let Some(path) = self.feeds_file.clone() else {
            self.status = "no config directory: feeds not saved".into();
            return;
        };
        if let Err(err) = config::save_feeds_to(&path, &self.feeds) {
            self.status = format!("could not save feeds: {err}");
        }
    }

    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Pane::Folders => Pane::Tracks,
            Pane::Tracks => Pane::Folders,
        };
    }

    /// Which table's rows sit under `pos`, if any.
    fn pane_at(&self, pos: Position) -> Option<Pane> {
        if self.folder_rows.contains(pos) {
            Some(Pane::Folders)
        } else if self.track_rows.contains(pos) {
            Some(Pane::Tracks)
        } else {
            None
        }
    }

    /// Absolute row index under `pos`, accounting for the table's scroll offset.
    fn row_at(&self, pane: Pane, pos: Position) -> Option<usize> {
        let (area, state, len) = match pane {
            Pane::Folders => (
                self.folder_rows,
                &self.folder_state,
                self.folder_row_count(),
            ),
            Pane::Tracks => (self.track_rows, &self.track_state, self.tracks.len()),
        };
        if !area.contains(pos) {
            return None;
        }
        let index = state.offset() + (pos.y - area.y) as usize;
        (index < len).then_some(index)
    }

    /// Left click: focus the pane and select the row. A second click on the same
    /// row plays it, so a single click never starts audio by accident.
    pub fn click(&mut self, pos: Position, now: Instant) {
        // An open prompt owns the screen; a stray click must not act behind it.
        if self.prompt.is_some() {
            return;
        }
        // Like a key, a click closes the key list and does nothing else.
        if self.show_keys {
            self.show_keys = false;
            return;
        }
        if self.add_area.contains(pos) {
            self.open_add_feed();
            return;
        }
        if self.remove_area.contains(pos) {
            self.remove_selected_feed();
            return;
        }
        if self.feed_rows.contains(pos) {
            self.focus = Pane::Folders;
            let row = self.feed_state.offset() + (pos.y - self.feed_rows.y) as usize;
            if row < self.feeds.len() && self.feed_state.selected() != Some(row) {
                self.feed_state.select(Some(row));
                self.reload_feed();
            }
            return;
        }
        for (tab, area) in Tab::ALL.iter().zip(self.tab_areas) {
            if area.contains(pos) {
                self.select_tab(*tab);
                return;
            }
        }
        if self.play_area.contains(pos) {
            self.toggle_play();
            return;
        }
        if self.prev_area.contains(pos) {
            self.prev_track();
            return;
        }
        if self.next_area.contains(pos) {
            self.next_track();
            return;
        }
        if self.seek_bar.contains(pos) {
            self.seek_to(self.bar_fraction(pos.x));
            return;
        }
        if self.shuffle_area.contains(pos) {
            self.toggle_shuffle();
            return;
        }
        if let Some(star) = self.star_areas.iter().position(|area| area.contains(pos))
            && let Some(track) = self.now_playing().cloned()
        {
            self.rate(&track, star as u8 + 1);
            return;
        }
        // The stars of a row in the list: that track, playing or not.
        if let Some(x) = self.track_star_x
            && self.track_rows.contains(pos)
            && (x..x + 3).contains(&pos.x)
            && let Some(row) = self.row_at(Pane::Tracks, pos)
            && let Some(track) = self.tracks.get(row).cloned()
            && library::remote_url(&track.path).is_some()
        {
            self.focus = Pane::Tracks;
            self.track_state.select(Some(row));
            self.rate(&track, (pos.x - x) as u8 + 1);
            return;
        }
        let Some(pane) = self.pane_at(pos) else {
            return;
        };
        let Some(index) = self.row_at(pane, pos) else {
            // Clicking empty space below the rows still moves focus.
            self.focus = pane;
            return;
        };

        let repeat = matches!(
            self.last_click,
            Some((p, i, at)) if p == pane && i == index && now.duration_since(at) < DOUBLE_CLICK
        );
        self.last_click = Some((pane, index, now));
        self.focus = pane;

        match pane {
            Pane::Folders => {
                self.select_folder(index);
                // A button, so one click is enough — like `+ Add feed`.
                if self.on_server_row() {
                    self.open_add_server();
                // Same gesture as a file manager: one click selects, two descends.
                } else if repeat {
                    self.enter_folder();
                }
            }
            Pane::Tracks => {
                self.track_state.select(Some(index));
                if repeat {
                    self.play_index(index);
                }
            }
        }
    }

    /// Scroll the pane under the cursor, which need not be the focused one.
    pub fn scroll(&mut self, pos: Position, delta: isize) {
        if self.feed_rows.contains(pos) {
            self.move_feed_selection(delta);
            return;
        }
        match self.pane_at(pos) {
            Some(Pane::Folders) => {
                let next = self.folder_state.selected().unwrap_or(0) as isize + delta;
                self.select_folder(
                    next.clamp(0, self.folder_row_count().saturating_sub(1) as isize) as usize,
                );
            }
            Some(Pane::Tracks) => {
                if !self.tracks.is_empty() {
                    let next = self.track_state.selected().unwrap_or(0) as isize + delta;
                    let last = self.tracks.len() as isize - 1;
                    self.track_state.select(Some(next.clamp(0, last) as usize));
                }
            }
            None => self.move_selection(delta),
        }
    }

    /// How far along the seek bar column `x` sits, as 0.0..=1.0.
    ///
    /// The left-most cell means 0.0 and the right-most means 1.0, so both ends of
    /// the track are actually reachable — dividing by the full width would make
    /// 1.0 unclickable.
    fn bar_fraction(&self, x: u16) -> f32 {
        let span = self.seek_bar.width.saturating_sub(1);
        if span == 0 {
            return 0.0;
        }
        let offset = x.saturating_sub(self.seek_bar.x).min(span);
        offset as f32 / span as f32
    }

    /// Drag on the seek bar: scrub without needing a fresh click each time. Only
    /// the row has to match — `bar_fraction` clamps the column, so dragging past
    /// either end pins to that end instead of stopping the scrub.
    pub fn drag(&mut self, pos: Position) {
        let bar = self.seek_bar;
        if bar.width == 0 || pos.y < bar.y || pos.y >= bar.bottom() {
            return;
        }
        self.seek_to(self.bar_fraction(pos.x));
    }

    pub fn seek_to(&mut self, fraction: f32) {
        let Some(total) = self.now_playing().and_then(|t| t.duration) else {
            return;
        };
        let target = total.mul_f32(fraction.clamp(0.0, 1.0));
        self.audio.seek(target);
        self.status = format!("seek {}", library::fmt_duration(target));
    }

    /// Nudge the playhead by `delta` seconds, clamped to the track.
    pub fn seek_by(&mut self, delta: i64) {
        let Some(total) = self.now_playing().and_then(|t| t.duration) else {
            return;
        };
        let now = self.audio.position().as_secs_f64();
        let target = (now + delta as f64).clamp(0.0, total.as_secs_f64());
        let target = Duration::from_secs_f64(target);
        self.audio.seek(target);
        self.status = format!("seek {}", library::fmt_duration(target));
    }

    /// Selecting a folder rescans it, so skip the work when nothing changed.
    fn select_folder(&mut self, row: usize) {
        if row >= self.folder_row_count() || self.folder_state.selected() == Some(row) {
            return;
        }
        self.folder_state.select(Some(row));
        self.touch_settings();
        self.reload_tracks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::picker::Picker;

    /// `seconds` of silence as a mono 16-bit PCM wav. Real audio, so playback and
    /// seeking actually run — an empty file only ever exercises the error path.
    pub(super) fn silent_wav(seconds: u32) -> Vec<u8> {
        const RATE: u32 = 8000;
        let data_len = RATE * 2 * seconds;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend(b"RIFF");
        out.extend((36 + data_len).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes()); // fmt chunk size
        out.extend(1u16.to_le_bytes()); // PCM
        out.extend(1u16.to_le_bytes()); // mono
        out.extend(RATE.to_le_bytes());
        out.extend((RATE * 2).to_le_bytes()); // byte rate
        out.extend(2u16.to_le_bytes()); // block align
        out.extend(16u16.to_le_bytes()); // bits per sample
        out.extend(b"data");
        out.extend(data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0);
        out
    }

    /// Two albums of short silent tracks.
    pub(super) struct Library(pub(super) PathBuf);

    impl Library {
        pub(super) fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("tuneterm-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let wav = silent_wav(2);
            for (album, tracks) in [("Alpha", 3), ("Beta", 2)] {
                let dir = root.join(album);
                std::fs::create_dir_all(&dir).unwrap();
                for i in 1..=tracks {
                    std::fs::write(dir.join(format!("{i:02} song.wav")), &wav).unwrap();
                }
            }
            // An artist folder holding two albums, for descending into.
            for (album, tracks) in [("Early", 2), ("Late", 4)] {
                let dir = root.join("Artist").join(album);
                std::fs::create_dir_all(&dir).unwrap();
                for i in 1..=tracks {
                    std::fs::write(dir.join(format!("{i:02} song.wav")), &wav).unwrap();
                }
            }
            Self(root)
        }

        /// Straight out of `App::new`, with the first scan still in flight —
        /// which is the state `main` restores into.
        fn raw_app(&self) -> App {
            App::new(
                self.0.clone(),
                Picker::halfblocks(),
                media::Bridge::detached(),
                Wake::none(),
            )
            .expect("app init")
        }

        /// Ready to assert on: the first listing has already landed.
        fn app(&self) -> App {
            let mut app = App::new(
                self.0.clone(),
                Picker::halfblocks(),
                media::Bridge::detached(),
                Wake::none(),
            )
            .expect("app init");
            app.wait_for_tracks();
            app
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Rows as the renderer would report them: 10 wide, starting at y = 1.
    fn rows(x: u16, y: u16, height: u16) -> Rect {
        Rect::new(x, y, 10, height)
    }

    /// The pane row of the folder called `name`, `..` accounted for.
    fn folder_row(app: &App, name: &str) -> usize {
        let index = app
            .folders
            .iter()
            .position(|folder| folder.label == name)
            .unwrap_or_else(|| panic!("no folder called {name}"));
        index + usize::from(app.shows_up_row())
    }

    /// Seeks are answered on another thread now, so a test that asserts on where
    /// the playhead ended up has to wait for that answer first.
    fn settled(app: &App) {
        assert_eq!(app.audio.wait_for_seek(), None, "seek failed");
    }

    /// Drive the loop the way `run` does until a restored track has been let go.
    ///
    /// Not `AudioPlayer::wait_for_seek`: that drains the answer `poll_seek` needs,
    /// so waiting on it first is exactly what would leave the track paused.
    fn settle_resume(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            app.poll_seek();
            if !app.resume_playing {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the restored track was never let go: {}", app.status);
    }

    /// The left pane lists one level, and empty branches never appear.
    #[test]
    fn lists_one_level_with_recursive_counts() {
        let lib = Library::new("scan");
        let app = lib.app();
        let seen: Vec<(&str, usize)> = app
            .folders
            .iter()
            .map(|f| (f.label.as_str(), f.count))
            .collect();
        assert_eq!(
            seen,
            vec![("Alpha", 3), ("Artist", 6), ("Beta", 2)],
            "counts must include subfolders"
        );
        assert_eq!(app.tracks.len(), 3, "the highlighted folder is listed");
    }

    /// The point of the change: highlighting a folder lists everything beneath it,
    /// not just its own files.
    #[test]
    fn the_listing_is_recursive() {
        let lib = Library::new("recursive");
        let mut app = lib.app();
        app.folder_state.select(Some(2)); // Artist/, which holds no files itself
        app.reload_tracks();
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 6, "both albums of the artist");
    }

    /// Drive the loop until a stream has opened or failed.
    fn wait_for_open(app: &mut App) {
        for _ in 0..400 {
            app.poll_open();
            if !app.is_opening() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the stream never opened");
    }

    /// A local library with a server added: one list at the root, local folders
    /// marked, server folders opened and left the same way as local ones, and a
    /// track played from the server.
    #[test]
    fn server_folders_sit_beside_local_ones() {
        let lib = Library::new("beside");
        let served = crate::server::TempDir::new("beside-served");
        let wav = silent_wav(2);
        for (album, name) in [("Gamma", "01 far.wav"), ("Alpha", "01 there.wav")] {
            let dir = served.0.join(album);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), &wav).unwrap();
        }
        let addr = crate::server::spawn_for_test(&served.0, Some("k"));

        let mut app = lib.app();
        let settings = lib.0.join("settings.txt");
        app.settings_file = Some(settings.clone());
        app.open_add_server();
        for c in format!("k@{addr}").chars() {
            app.prompt_key(c);
        }
        app.submit_prompt();
        assert!(
            app.prompt.is_none(),
            "{:?}",
            app.prompt.as_ref().map(|p| &p.hint)
        );

        // Alpha is in both: shown once, as the local one.
        let labels: Vec<_> = app.folders.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["Alpha", "Artist", "Beta", "Gamma"]);
        let marked: Vec<_> = app
            .folders
            .iter()
            .filter(|f| app.can_move(f))
            .map(|f| f.label.as_str())
            .collect();
        assert_eq!(marked, ["Alpha", "Artist", "Beta"], "local ones are marked");
        assert!(
            app.folders.iter().all(|f| f.newest > 0),
            "every folder has a time, the server's included"
        );

        // Kept, with the token apart from the address.
        let saved = config::load_settings_from(&settings).remote;
        assert_eq!(saved.server, Some(format!("tuneterm://{addr}")));
        assert_eq!(saved.token.as_deref(), Some("k"));

        // Into the server's folder, and back out onto the same row.
        let row = folder_row(&app, "Gamma");
        app.select_folder(row);
        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 1, "a leaf lists its own tracks");
        assert_eq!(app.focus, Pane::Tracks, "a leaf moves focus instead");
        app.play_index(0);
        wait_for_open(&mut app);
        assert!(app.status.starts_with("playing"), "{}", app.status);
        assert_eq!(
            app.now_playing().map(|t| t.path.clone()),
            Some(PathBuf::from(format!("tuneterm://{addr}/Gamma/01 far.wav")))
        );
    }

    /// `u` uploads the folder and deletes it here; the root then lists
    /// the server's copy in its place.
    #[test]
    fn moving_a_folder_puts_it_on_the_server_and_takes_it_off_here() {
        let lib = Library::new("move");
        let served = crate::server::TempDir::new("move-served");
        let addr = crate::server::spawn_for_test(&served.0, Some("k"));
        let mut app = lib.app();
        app.settings_file = None;
        app.remote = config::Remote {
            server: Some(format!("tuneterm://{addr}")),
            token: Some("k".into()),
        };

        app.folder_state.select(Some(folder_row(&app, "Beta")));
        app.move_selected();
        assert!(app.prompt.is_none(), "no question asked");
        app.wait_for_move();

        assert!(app.status.starts_with("moved Beta"), "{}", app.status);
        let log = app
            .transfer_view()
            .expect("the log stays up after the move");
        assert_eq!((log.files_done, log.files), (2, 2));
        assert_eq!(log.bytes_done, log.bytes);
        assert!(log.finished.is_some() && log.sending.is_empty());
        assert_eq!(
            log.log,
            [LogLine::Note("deleted 2 files here".into())],
            "files that arrived are not listed, only how it ended"
        );
        assert!(app.hide_transfer_log(), "Escape closes the log first");
        assert!(app.transfer_view().is_none());
        assert!(!app.hide_transfer_log(), "and then it is Escape again");
        assert!(!lib.0.join("Beta").exists(), "the local copy is gone");
        for name in ["01 song.wav", "02 song.wav"] {
            assert!(
                served.0.join("Beta").join(name).is_file(),
                "{name} not on the server"
            );
        }
        let beta = app
            .folders
            .iter()
            .find(|f| f.label == "Beta")
            .expect("Beta still listed, from the server");
        assert!(library::remote_url(&beta.path).is_some());
        assert!(!app.can_move(beta));

        // A server folder is not offered for a move.
        app.folder_state.select(Some(folder_row(&app, "Beta")));
        app.move_selected();
        assert!(!app.is_moving(), "a server folder is not moved");
    }

    /// The session as it looked in the report: at the root, an artist
    /// highlighted, a track of theirs playing, and a server added. It must come
    /// back playing.
    #[test]
    fn a_playing_track_comes_back_playing_with_a_server_added() {
        for reachable in [true, false] {
            let lib = Library::new(&format!("resume-server-{reachable}"));
            let served = crate::server::TempDir::new("resume-served");
            let server = if reachable {
                format!(
                    "tuneterm://{}",
                    crate::server::spawn_for_test(&served.0, None)
                )
            } else {
                "tuneterm://127.0.0.1:1".to_string()
            };
            let track = lib.0.join("Beta").join("02 song.wav");
            let settings = config::Settings {
                session: config::Session {
                    tab: Some("local".into()),
                    folder: Some(lib.0.clone()),
                    selected: Some(lib.0.join("Beta")),
                    track: Some(track.to_string_lossy().into_owned()),
                    position: Duration::from_millis(500),
                    playing: true,
                    ..Default::default()
                },
                remote: config::Remote {
                    server: Some(server),
                    token: None,
                },
                ..Default::default()
            };

            let mut app = lib.raw_app();
            app.settings_file = None;
            app.restore(settings);
            app.wait_for_tracks();
            settle_resume(&mut app);
            assert_eq!(
                app.now_playing().map(|t| t.path.clone()),
                Some(track.clone()),
                "reachable={reachable}: nothing came back: {}",
                app.status
            );
            assert!(
                !app.audio.is_paused(),
                "reachable={reachable}: came back paused"
            );
        }
    }

    /// A folder moved to the server while its track was playing: the session names
    /// the local path, which is gone, and the track comes back from the server.
    #[test]
    fn a_track_moved_to_the_server_comes_back_from_there() {
        let lib = Library::new("resume-moved");
        let served = crate::server::TempDir::new("resume-moved-served");
        let addr = crate::server::spawn_for_test(&served.0, None);
        std::fs::create_dir_all(served.0.join("Beta")).unwrap();
        for name in ["01 song.wav", "02 song.wav"] {
            std::fs::rename(
                lib.0.join("Beta").join(name),
                served.0.join("Beta").join(name),
            )
            .unwrap();
        }
        std::fs::remove_dir(lib.0.join("Beta")).unwrap();

        let settings = config::Settings {
            session: config::Session {
                tab: Some("local".into()),
                folder: Some(lib.0.clone()),
                selected: Some(lib.0.join("Beta")),
                track: Some(
                    lib.0
                        .join("Beta/02 song.wav")
                        .to_string_lossy()
                        .into_owned(),
                ),
                position: Duration::from_millis(500),
                playing: true,
                ..Default::default()
            },
            remote: config::Remote {
                server: Some(format!("tuneterm://{addr}")),
                token: None,
            },
            ..Default::default()
        };
        let mut app = lib.raw_app();
        app.settings_file = None;
        app.restore(settings);
        app.wait_for_tracks();
        wait_for_open(&mut app);
        settle_resume(&mut app);

        assert_eq!(
            app.selected_folder().map(|f| f.path.clone()),
            Some(PathBuf::from(format!("tuneterm://{addr}/Beta"))),
            "the highlight followed the folder to the server"
        );
        assert_eq!(
            app.now_playing().map(|t| t.path.clone()),
            Some(PathBuf::from(format!("tuneterm://{addr}/Beta/02 song.wav"))),
            "{}",
            app.status
        );
        assert!(!app.audio.is_paused(), "came back paused: {}", app.status);
    }

    /// `r`, `m` and `x` on local things: a folder renamed in place, a track moved
    /// into another folder, a folder deleted only once `y` says so.
    #[test]
    fn local_things_are_renamed_moved_and_deleted() {
        let lib = Library::new("edit-local");
        let mut app = lib.app();
        let type_in = |app: &mut App, text: &str| {
            if let Some(prompt) = app.prompt.as_mut() {
                prompt.input.clear();
            }
            for c in text.chars() {
                app.prompt_key(c);
            }
            app.submit_prompt();
        };

        app.select_folder(folder_row(&app, "Beta"));
        app.wait_for_tracks();
        app.ask_rename();
        assert_eq!(app.prompt.as_ref().map(|p| p.input.as_str()), Some("Beta"));
        type_in(&mut app, "Gamma");
        assert!(
            app.prompt.is_none(),
            "{:?}",
            app.prompt.as_ref().map(|p| &p.hint)
        );
        assert!(lib.0.join("Gamma").is_dir() && !lib.0.join("Beta").exists());
        app.wait_for_tracks();

        // A track, from the right pane, into another folder that does not exist yet.
        app.select_folder(folder_row(&app, "Gamma"));
        app.wait_for_tracks();
        app.focus = Pane::Tracks;
        app.track_state.select(Some(0));
        app.ask_relocate();
        assert_eq!(
            app.prompt.as_ref().map(|p| p.input.as_str()),
            Some("Gamma/01 song.wav")
        );
        type_in(&mut app, "Singles/01 song.wav");
        assert!(lib.0.join("Singles/01 song.wav").is_file());

        // Onto something that exists: refused, field kept open.
        app.track_state.select(Some(0));
        app.ask_relocate();
        type_in(&mut app, "Alpha/01 song.wav");
        assert!(
            app.prompt
                .as_ref()
                .is_some_and(|p| p.hint.contains("exists"))
        );
        app.cancel_prompt();

        // Delete waits for `y`: Enter and any other key do nothing.
        app.focus = Pane::Folders;
        app.select_folder(folder_row(&app, "Gamma"));
        app.ask_delete();
        app.submit_prompt();
        app.prompt_key('n');
        assert!(lib.0.join("Gamma").is_dir(), "deleted without a y");
        app.prompt_key('y');
        assert!(app.prompt.is_none());
        assert!(!lib.0.join("Gamma").exists(), "{}", app.status);
    }

    /// The same keys on a server folder go through the server.
    #[test]
    fn server_things_are_renamed_and_deleted() {
        let lib = Library::new("edit-remote");
        let served = crate::server::TempDir::new("edit-remote-served");
        std::fs::create_dir_all(served.0.join("Far")).unwrap();
        std::fs::write(served.0.join("Far/01.wav"), silent_wav(1)).unwrap();
        let addr = crate::server::spawn_for_test(&served.0, Some("k"));
        let mut app = lib.app();
        app.settings_file = None;
        app.remote = config::Remote {
            server: Some(format!("tuneterm://{addr}")),
            token: Some("k".into()),
        };
        remote::Server::connect(&format!("tuneterm://{addr}"), Some("k")).unwrap();
        app.relist_root();

        app.select_folder(folder_row(&app, "Far"));
        app.ask_rename();
        app.prompt.as_mut().unwrap().input = "Near".into();
        app.submit_prompt();
        assert!(
            app.prompt.is_none(),
            "{:?}",
            app.prompt.as_ref().map(|p| &p.hint)
        );
        assert!(served.0.join("Near/01.wav").is_file());
        assert!(
            app.folders.iter().any(|f| f.label == "Near"),
            "not listed again"
        );

        app.select_folder(folder_row(&app, "Near"));
        app.ask_delete();
        app.prompt_key('y');
        assert!(!served.0.join("Near").exists(), "{}", app.status);
    }

    /// Quit after the cursor wandered off the playing album: the restart goes
    /// back to the album — highlighted in the folder above it, not entered — and
    /// the track carries on.
    #[test]
    fn a_restart_goes_back_to_what_was_playing_not_to_the_cursor() {
        let lib = Library::new("resume-wandered");
        let file = lib.0.join("settings.txt");

        let mut before = lib.app();
        before.settings_file = Some(file.clone());
        before.select_folder(folder_row(&before, "Artist"));
        before.wait_for_tracks();
        before.enter_folder();
        before.wait_for_tracks();
        before.select_folder(folder_row(&before, "Late"));
        before.wait_for_tracks();
        before.play_index(1);
        let track = before.now_playing().expect("playing").path.clone();

        // Wander: up to the root, onto another album.
        before.leave_folder();
        before.select_folder(folder_row(&before, "Beta"));
        before.wait_for_tracks();
        before.save_settings(true);

        let mut after = lib.raw_app();
        after.restore(config::load_settings_from(&file));
        after.wait_for_tracks();
        settle_resume(&mut after);

        assert_eq!(
            after.cwd,
            lib.0.join("Artist"),
            "the folder that holds the album"
        );
        assert_eq!(
            after.selected_folder().map(|f| f.label.as_str()),
            Some("Late"),
            "the cursor is on the album, not inside it"
        );
        assert!(after.can_leave(), "the way back up to the root is there");
        assert_eq!(after.now_playing().map(|t| t.path.clone()), Some(track));
        assert!(!after.audio.is_paused(), "{}", after.status);
    }

    /// A mistyped address is refused where it was typed, and an unreachable one
    /// is only added when asked twice.
    #[test]
    fn a_server_address_is_checked_before_it_is_kept() {
        let lib = Library::new("check-server");
        let mut app = lib.app();
        app.settings_file = None;
        let type_in = |app: &mut App, text: &str| {
            app.open_add_server();
            for c in text.chars() {
                app.prompt_key(c);
            }
            app.submit_prompt();
        };

        type_in(&mut app, "simple_token&192.168.0.200:7700");
        let hint = app
            .prompt
            .as_ref()
            .map(|p| p.hint.clone())
            .expect("kept open");
        assert!(hint.contains('@'), "{hint}");
        assert_eq!(app.remote.server, None);
        app.cancel_prompt();

        // Nothing listens on port 1: refused at once, kept open to fix.
        type_in(&mut app, "tok@127.0.0.1:1");
        let hint = app
            .prompt
            .as_ref()
            .map(|p| p.hint.clone())
            .expect("kept open");
        assert!(hint.contains("anyway"), "{hint}");
        assert_eq!(app.remote.server, None);
        app.submit_prompt();
        assert!(app.prompt.is_none(), "a second Enter adds it anyway");
        assert_eq!(app.remote.server.as_deref(), Some("tuneterm://127.0.0.1:1"));
        assert_eq!(app.remote.token.as_deref(), Some("tok"));
    }

    /// The button at the end of the root: a click or Enter opens the field.
    #[test]
    fn the_server_button_opens_the_field() {
        let lib = Library::new("button");
        let mut app = lib.app();
        let button = app.folder_row_count() - 1;

        app.folder_state.select(Some(button));
        assert!(app.on_server_row());
        app.enter_selected();
        assert!(matches!(
            app.prompt.as_ref().map(|p| &p.kind),
            Some(PromptKind::Server)
        ));
        app.cancel_prompt();

        app.folder_state.select(Some(0));
        app.folder_rows = rows(0, 1, 10);
        app.click(
            Position {
                x: 2,
                y: 1 + button as u16,
            },
            Instant::now(),
        );
        assert!(
            matches!(
                app.prompt.as_ref().map(|p| &p.kind),
                Some(PromptKind::Server)
            ),
            "one click is enough for a button"
        );
    }

    /// Given a server address as the folder, the player browses that server alone.
    #[test]
    fn a_server_as_the_root_browses_like_a_folder() {
        let lib = Library::new("served");
        let addr = crate::server::spawn_for_test(&lib.0, None);
        let mut app = App::new(
            PathBuf::from(format!("tuneterm://{addr}")),
            Picker::halfblocks(),
            media::Bridge::detached(),
            Wake::none(),
        )
        .expect("app init");
        app.wait_for_tracks();

        let labels: Vec<_> = app.folders.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["Alpha", "Artist", "Beta"]);
        assert_eq!(app.here(), addr.to_string());

        app.folder_state.select(Some(folder_row(&app, "Artist")));
        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, PathBuf::from(format!("tuneterm://{addr}/Artist")));
        assert_eq!(app.here(), format!("{addr} / Artist"));
        assert_eq!(app.tracks.len(), 2, "the first album, Early, is listed");

        app.leave_folder();
        assert_eq!(
            app.selected_folder().map(|f| f.label.as_str()),
            Some("Artist")
        );
    }

    #[test]
    fn enter_descends_and_backspace_returns_to_the_same_row() {
        let lib = Library::new("descend");
        let mut app = lib.app();
        app.folder_state.select(Some(2)); // Artist/
        app.reload_tracks();
        app.wait_for_tracks();

        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0.join("Artist"));
        let inside: Vec<&str> = app.folders.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(inside, vec!["Early", "Late"]);
        assert_eq!(app.tracks.len(), 2, "Early is highlighted");
        assert!(app.can_leave());

        app.leave_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0);
        assert_eq!(app.folder_state.selected(), Some(2), "row restored");
        assert!(!app.can_leave(), "the root is the floor");
    }

    /// `..` is a row of the table, so every index between a click and a folder has to
    /// account for it. This is the mapping, checked at both levels.
    #[test]
    fn the_up_row_shifts_folder_indices() {
        let lib = Library::new("up-row");
        let mut app = lib.app();

        // At the root `..` climbs nowhere but is still there, listing the whole
        // library — and the server button closes the list.
        assert!(app.shows_up_row());
        assert!(app.shows_server_row());
        assert_eq!(app.folder_row_count(), app.folders.len() + 2);
        assert!(
            app.folder_at(app.folders.len() + 1).is_none(),
            "the last row is the button"
        );
        assert!(app.folder_at(0).is_none(), "row 0 is `..`, not a folder");
        assert_eq!(app.folder_at(1).map(|f| f.label.as_str()), Some("Alpha"));
        assert_eq!(
            app.folder_state.selected(),
            Some(1),
            "starts on the first folder"
        );
        assert!(!app.on_up_row());

        app.folder_state.select(Some(2)); // Artist/
        app.reload_tracks();
        app.wait_for_tracks();
        app.enter_folder();
        app.wait_for_tracks();

        assert!(app.shows_up_row());
        assert!(!app.shows_server_row(), "the button is only at the root");
        assert_eq!(app.folder_row_count(), app.folders.len() + 1);
        assert!(app.folder_at(0).is_none(), "row 0 is `..`, not a folder");
        assert_eq!(app.folder_at(1).map(|f| f.label.as_str()), Some("Early"));
        assert_eq!(
            app.folder_state.selected(),
            Some(1),
            "landed on the first folder, not on `..`"
        );
        assert!(!app.on_up_row());
    }

    /// Enter on `..` climbs, the same as Backspace.
    #[test]
    fn enter_on_the_up_row_climbs() {
        let lib = Library::new("enter-up");
        let mut app = lib.app();
        app.folder_state.select(Some(2));
        app.reload_tracks();
        app.wait_for_tracks();
        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0.join("Artist"));

        app.folder_state.select(Some(0)); // `..`
        assert!(app.on_up_row());
        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0, "Enter on `..` went up");
    }

    /// Highlighting `..` still lists something useful: everything under the folder
    /// you are standing in.
    #[test]
    fn the_up_row_lists_the_current_folder() {
        let lib = Library::new("up-listing");
        let mut app = lib.app();
        app.folder_state.select(Some(2)); // Artist/
        app.reload_tracks();
        app.wait_for_tracks();
        app.enter_folder();
        app.wait_for_tracks();

        app.select_folder(0); // `..`
        app.wait_for_tracks();
        assert_eq!(app.listing_dir(), lib.0.join("Artist"));
        assert_eq!(app.tracks.len(), 6, "both albums of the artist");
    }

    /// Two clicks on a folder descend, the way a file manager behaves. One must not.
    #[test]
    fn double_clicking_a_folder_descends() {
        let lib = Library::new("dbl-folder");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);

        let at = Position { x: 2, y: 3 }; // row 2 == Artist/
        let now = Instant::now();
        app.click(at, now);
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0, "one click must not descend");

        app.click(at, now + Duration::from_millis(100));
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0.join("Artist"), "two clicks descend");
    }

    /// Slow clicks are two separate selections, not a descent.
    #[test]
    fn two_slow_clicks_do_not_descend() {
        let lib = Library::new("slow-folder");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);

        let at = Position { x: 2, y: 2 };
        let now = Instant::now();
        app.click(at, now);
        app.click(at, now + Duration::from_secs(2));
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0);
    }

    /// The `..` row must be clickable, since that is the point of showing it.
    #[test]
    fn double_clicking_the_up_row_climbs() {
        let lib = Library::new("dbl-up");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);
        app.folder_state.select(Some(2));
        app.reload_tracks();
        app.wait_for_tracks();
        app.enter_folder();
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0.join("Artist"));

        let up = Position { x: 2, y: 1 }; // row 0 == `..`
        let now = Instant::now();
        app.click(up, now);
        app.click(up, now + Duration::from_millis(100));
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0);
    }

    /// A leaf album has nothing below it, so Enter moves to the tracks instead of
    /// leaving the pane empty.
    #[test]
    fn entering_a_leaf_moves_focus_instead() {
        let lib = Library::new("leaf");
        let mut app = lib.app();
        app.folder_state.select(Some(1)); // Alpha/, files only
        app.reload_tracks();
        app.wait_for_tracks();

        app.enter_folder();
        assert_eq!(app.cwd, lib.0, "did not descend");
        assert_eq!(app.focus, Pane::Tracks);
    }

    #[test]
    fn leaving_the_root_does_nothing() {
        let lib = Library::new("floor");
        let mut app = lib.app();
        app.leave_folder();
        assert_eq!(app.cwd, lib.0);
    }

    /// Browsing must not derail playback: the queue is a snapshot, so next and
    /// previous keep walking the album you started, not the folder under the cursor.
    #[test]
    fn browsing_does_not_hijack_the_queue() {
        let lib = Library::new("queue");
        let mut app = lib.app();
        app.play_index(0);
        let started = app.now_playing().map(|t| t.path.clone());
        assert!(started.is_some(), "playback failed: {}", app.status);

        // Wander off to a different folder entirely.
        app.folder_state.select(Some(2));
        app.reload_tracks();
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 6, "now listing the artist");
        assert_eq!(
            app.now_playing().map(|t| t.path.clone()),
            started,
            "still the same track"
        );
        assert_eq!(app.playing_row(), None, "not visible in this listing");

        app.next_track();
        let next = app.now_playing().expect("next failed").path.clone();
        assert!(
            next.starts_with(lib.0.join("Alpha")),
            "next left the queue: {next:?}"
        );
    }

    /// A folder that changed since it was read is read again once the folder list
    /// is rebuilt — a file added, or one rewritten in place with the count the same,
    /// the way a tag edit is.
    #[test]
    fn a_changed_folder_is_not_served_from_memory() {
        /// Browsing, which is what lists the root again — unlike a refresh, which
        /// forgets everything anyway.
        fn in_and_out_of_artist(app: &mut App) {
            app.select_folder(2);
            app.wait_for_tracks();
            app.enter_folder();
            app.wait_for_tracks();
            app.leave_folder();
            app.wait_for_tracks();
        }

        let lib = Library::new("memo-stale");
        let mut app = lib.app(); // Alpha is listed, and remembered
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 3);

        std::fs::write(lib.0.join("Alpha").join("04 new.wav"), silent_wav(1)).unwrap();
        in_and_out_of_artist(&mut app);
        app.select_folder(1);
        assert!(app.tracks_loading, "Alpha grew, yet came from the memo");
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 4);

        // Same count, newer file.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(lib.0.join("Alpha").join("01 song.wav"), silent_wav(3)).unwrap();
        in_and_out_of_artist(&mut app);
        app.select_folder(1);
        assert!(app.tracks_loading, "a rewritten file went unnoticed");
        app.wait_for_tracks();

        // And unchanged, it is remembered again.
        in_and_out_of_artist(&mut app);
        app.select_folder(1);
        assert!(!app.tracks_loading, "an unchanged folder was read again");
    }

    /// A second visit to a folder must come from the memo, not another scan.
    #[test]
    fn revisiting_a_folder_is_served_from_memory() {
        let lib = Library::new("memo");
        let mut app = lib.app();
        app.folder_state.select(Some(2));
        app.reload_tracks();
        app.wait_for_tracks();

        app.folder_state.select(Some(1));
        app.reload_tracks();
        assert!(!app.tracks_loading, "Alpha should have been remembered");
        assert_eq!(app.tracks.len(), 3);
    }

    /// Moving onto a remembered folder while a scan of the previous one is still
    /// running must not let that scan land here — neither on screen nor, worse, in
    /// the memo, where it would answer for the wrong folder from then on.
    #[test]
    fn a_scan_in_flight_cannot_land_on_the_folder_moved_to() {
        let lib = Library::new("stale-scan");
        let mut app = lib.app(); // Alpha is listed, and now remembered

        app.select_folder(2); // Artist/, 6 tracks — the scan is in flight
        app.select_folder(1); // straight back to Alpha, served from the memo
        assert_eq!(app.tracks.len(), 3, "Alpha, from the memo");

        // Every chance for the Artist scan to finish and be picked up.
        for _ in 0..20 {
            app.poll_tracks();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.tracks.len(), 3, "Artist's scan landed on Alpha");

        // And the memo still has to answer for the right folders.
        app.select_folder(2);
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 6, "Artist");
        app.select_folder(1);
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 3, "Alpha, remembered as Artist's tracks");
    }

    /// A track that streams, pointed at a port nothing is listening on.
    fn remote_track(url: &str) -> Track {
        Track {
            path: PathBuf::from(url),
            title: "episode".into(),
            artist: "feed".into(),
            album: "feed".into(),
            duration: None,
            url: Some(url.to_string()),
            art_url: None,
            stars: None,
        }
    }

    /// Enter on an episode used to open the stream on this thread: a length probe,
    /// a range request and a header read, up to `net::TIMEOUT` of them against a
    /// host that never answers, with the interface frozen throughout.
    #[test]
    fn starting_a_remote_track_returns_at_once() {
        let lib = Library::new("remote-open");
        let mut app = lib.app();
        app.queue = vec![remote_track("http://127.0.0.1:9/never.mp3")];

        let started = Instant::now();
        app.play_queue_index(0);
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_millis(250),
            "play_queue_index blocked for {waited:?}"
        );
        assert!(app.is_opening(), "the open should still be in flight");
        assert!(app.status.starts_with("opening"), "{}", app.status);
    }

    /// Nothing is queued while a stream opens, and that silence is not a track
    /// that ended — reading it as one would race through the whole queue.
    #[test]
    fn a_stream_still_opening_is_not_a_finished_track() {
        let lib = Library::new("opening-tick");
        let mut app = lib.app();
        app.queue = vec![
            remote_track("http://127.0.0.1:9/one.mp3"),
            remote_track("http://127.0.0.1:9/two.mp3"),
        ];

        app.play_queue_index(0);
        assert!(app.audio.is_finished(), "nothing is playing yet");
        for _ in 0..5 {
            app.tick();
        }
        assert_eq!(
            app.queue_pos,
            Some(0),
            "tick walked off a track still opening"
        );
    }

    /// Stopping has to let go of a pending open, or it would land afterwards and
    /// start playing on its own.
    #[test]
    fn stopping_abandons_a_stream_that_is_still_opening() {
        let lib = Library::new("stop-opening");
        let mut app = lib.app();
        app.queue = vec![remote_track("http://127.0.0.1:9/one.mp3")];

        app.play_queue_index(0);
        assert!(app.is_opening());
        app.stop_playback();

        assert!(!app.is_opening(), "the open outlived the stop");
        assert_eq!(app.queue_pos, None);
    }

    #[test]
    fn clicking_a_track_row_selects_it_without_playing() {
        let lib = Library::new("click-track");
        let mut app = lib.app();
        app.track_rows = rows(30, 1, 10);

        app.click(Position { x: 32, y: 3 }, Instant::now());

        assert_eq!(app.focus, Pane::Tracks, "focus follows the click");
        assert_eq!(app.track_state.selected(), Some(2), "third row");
        assert!(
            !app.is_playing_something(),
            "one click must not start audio"
        );
    }

    #[test]
    fn clicking_a_folder_row_switches_album() {
        let lib = Library::new("click-folder");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);

        app.click(Position { x: 2, y: 3 }, Instant::now());
        app.wait_for_tracks();

        assert_eq!(app.focus, Pane::Folders);
        assert_eq!(app.folder_state.selected(), Some(2), "third row");
        // Row 2 is Artist/, whose six tracks live in two subfolders.
        assert_eq!(app.tracks.len(), 6, "listed recursively");
    }

    /// Clicking past the last row should move focus but not change the selection.
    #[test]
    fn clicking_empty_space_only_moves_focus() {
        let lib = Library::new("click-empty");
        let mut app = lib.app();
        app.track_rows = rows(30, 1, 10);
        app.track_state.select(Some(0));

        app.click(Position { x: 32, y: 9 }, Instant::now());

        assert_eq!(app.focus, Pane::Tracks);
        assert_eq!(app.track_state.selected(), Some(0), "selection unchanged");
    }

    #[test]
    fn scroll_targets_the_pane_under_the_cursor_not_the_focused_one() {
        let lib = Library::new("scroll");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);
        app.track_rows = rows(30, 1, 10);
        app.focus = Pane::Folders;

        // Cursor over the tracks pane while folders has focus.
        app.scroll(Position { x: 32, y: 2 }, 1);
        app.wait_for_tracks();

        assert_eq!(app.track_state.selected(), Some(1), "tracks scrolled");
        assert_eq!(
            app.folder_state.selected(),
            Some(1),
            "focused pane untouched"
        );
    }

    /// Both ends of the track must be reachable: the last column has to map to
    /// 1.0, which dividing by the full width would never produce.
    #[test]
    fn seek_bar_maps_ends_to_zero_and_one() {
        let lib = Library::new("seek-ends");
        let mut app = lib.app();
        app.seek_bar = Rect::new(20, 5, 11, 1);

        assert_eq!(app.bar_fraction(20), 0.0, "left edge");
        assert_eq!(app.bar_fraction(30), 1.0, "right edge");
        assert_eq!(app.bar_fraction(25), 0.5, "middle");
    }

    /// Clicks and drags outside the bar must not move the playhead.
    #[test]
    fn seek_bar_clamps_out_of_range_columns() {
        let lib = Library::new("seek-clamp");
        let mut app = lib.app();
        app.seek_bar = Rect::new(20, 5, 11, 1);

        assert_eq!(app.bar_fraction(0), 0.0, "left of the bar");
        assert_eq!(app.bar_fraction(999), 1.0, "right of the bar");
    }

    /// A one-cell bar has no span to divide by.
    #[test]
    fn degenerate_seek_bar_does_not_divide_by_zero() {
        let lib = Library::new("seek-degenerate");
        let mut app = lib.app();
        app.seek_bar = Rect::new(4, 4, 1, 1);
        assert_eq!(app.bar_fraction(4), 0.0);
    }

    /// End-to-end: a click on the bar must actually move the playhead, not just
    /// report success. rodio silently no-ops a seek when nothing is queued.
    #[test]
    fn clicking_the_seek_bar_moves_the_playhead() {
        let lib = Library::new("seek-live");
        let mut app = lib.app();
        app.play_index(0);
        assert!(
            app.is_playing_something(),
            "playback failed: {}",
            app.status
        );

        app.seek_bar = Rect::new(0, 30, 11, 1);
        // Middle of the bar on a 2 s track.
        app.click(Position { x: 5, y: 30 }, Instant::now());
        settled(&app);

        // Playback keeps running, so the bounds are generous on purpose — the
        // claim is "it jumped to the middle", not an exact sample offset.
        let pos = app.audio.position();
        assert!(
            pos >= Duration::from_millis(600) && pos <= Duration::from_millis(1600),
            "expected ~1s, got {pos:?} ({})",
            app.status
        );
    }

    #[test]
    fn seek_by_clamps_to_the_track() {
        let lib = Library::new("seek-by");
        let mut app = lib.app();
        app.play_index(0);
        assert!(
            app.is_playing_something(),
            "playback failed: {}",
            app.status
        );

        // Backwards first: rodio accepts a seek only while a source is queued, and
        // jumping to the very end drains it (`Player::try_seek` returns Ok without
        // seeking when `sound_count == 0`). In the app `tick()` moves to the next
        // track at that point, so the dead state never lingers.
        app.seek_to(0.75);
        settled(&app);
        app.seek_by(-600); // far before the start
        settled(&app);
        let at_start = app.audio.position();
        assert!(
            at_start < Duration::from_millis(500),
            "should have clamped to the start, got {at_start:?}"
        );

        app.seek_by(600); // far past the 2 s end
        settled(&app);
        let at_end = app.audio.position();
        assert!(
            at_end >= Duration::from_millis(1500),
            "should have clamped to the end, got {at_end:?}"
        );
    }

    /// A volume set in one run has to be there in the next one.
    #[test]
    fn the_volume_survives_a_restart() {
        let lib = Library::new("volume-persist");
        let mut app = lib.app();
        let file = std::env::temp_dir().join(format!("tuneterm-vol-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        app.settings_file = Some(file.clone());

        // Explicit rather than relative: `App::new` picks up whatever the machine
        // running the tests already had saved.
        app.audio.set_volume(0.8);
        app.nudge_volume(-0.4);
        let set = app.audio.volume();
        assert!((set - 0.4).abs() < 1e-6, "the volume did not move: {set}");

        // Nothing is written until it settles, so that a held-down key does not
        // put a file write behind every repeat.
        app.save_settings(false);
        assert!(!file.exists(), "wrote before the volume had settled");

        // Quitting cannot wait for that.
        app.save_settings(true);
        assert_eq!(config::load_settings_from(&file).volume.get(), set);

        let _ = std::fs::remove_file(&file);
    }

    /// The whole point: quit deep in a library, on a track, and come back to it.
    #[test]
    fn a_session_is_saved_and_put_back() {
        let lib = Library::new("session-round-trip");
        let file = std::env::temp_dir().join(format!("tuneterm-sess-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        // Browse into Artist, land on one of its albums, and play a track.
        let mut before = lib.app();
        before.settings_file = Some(file.clone());
        before.select_folder(folder_row(&before, "Artist"));
        before.wait_for_tracks();
        before.enter_folder();
        before.wait_for_tracks();
        let cwd = before.cwd.clone();
        let listing = before.listing_dir();
        assert_eq!(cwd, lib.0.join("Artist"), "did not descend");

        before.play_index(0);
        let track = before.now_playing().expect("playing").path.clone();
        before.seek_to(0.5);
        assert_eq!(before.audio.wait_for_seek(), None);
        let at = before.audio.position();
        before.save_settings(true);

        // A fresh app is at the root and playing nothing until it restores.
        let mut after = lib.app();
        assert_eq!(after.cwd, lib.0);
        assert!(!after.is_playing_something());

        after.restore(config::load_settings_from(&file));
        after.wait_for_tracks();

        assert_eq!(after.cwd, cwd, "did not come back to the folder");
        assert_eq!(after.listing_dir(), listing, "lost the highlighted row");
        assert!(
            after.can_leave(),
            "the trail was not rebuilt, Backspace is dead"
        );
        assert_eq!(
            after.now_playing().map(|t| t.path.clone()),
            Some(track),
            "the track did not come back: {}",
            after.status
        );
        assert!(
            after.audio.is_paused(),
            "a restored track must not start itself"
        );
        assert_eq!(after.audio.wait_for_seek(), None);
        let back = after.audio.position();
        assert!(
            back.abs_diff(at) < Duration::from_millis(400),
            "resumed at {back:?}, left at {at:?}"
        );

        let _ = std::fs::remove_file(&file);
    }

    /// Paused when you quit means paused when you come back — the restore brings
    /// the state back, it does not pick one.
    #[test]
    fn a_track_that_was_paused_comes_back_paused() {
        let lib = Library::new("session-was-paused");
        let file = std::env::temp_dir().join(format!("tuneterm-sess-p-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        let mut before = lib.app();
        before.settings_file = Some(file.clone());
        before.play_index(0);
        before.toggle_play();
        assert!(before.audio.is_paused(), "did not pause");
        before.save_settings(true);
        assert!(!config::load_settings_from(&file).session.playing);

        let mut after = lib.app();
        after.restore(config::load_settings_from(&file));
        after.wait_for_tracks();
        settle_resume(&mut after);
        assert!(after.is_playing_something(), "{}", after.status);
        assert!(after.audio.is_paused(), "started itself: {}", after.status);
    }

    /// A track held quiet while its seek is in flight has to be let go once the
    /// playhead has moved, or restoring a playing track leaves silence.
    #[test]
    fn a_restored_track_is_only_let_go_once_the_playhead_has_moved() {
        let lib = Library::new("session-let-go");
        let mut app = lib.app();
        let track = lib.0.join("Alpha").join("01 song.wav");
        app.restore(config::Settings {
            session: config::Session {
                folder: Some(lib.0.join("Alpha")),
                track: Some(track.to_string_lossy().into_owned()),
                position: Duration::from_millis(900),
                playing: true,
                ..Default::default()
            },
            ..Default::default()
        });
        app.wait_for_tracks();

        // Held until the seek is answered, so the first thing heard is the right
        // part of the track rather than a click of its opening. Nothing but
        // `poll_seek` releases it, so this is a fact and not a race.
        assert!(app.audio.is_paused(), "played before it had seeked");

        settle_resume(&mut app);
        assert!(!app.audio.is_paused(), "never let go: {}", app.status);
        assert!(
            app.audio.position() >= Duration::from_millis(700),
            "let go before the playhead moved: {:?}",
            app.audio.position()
        );
    }

    /// A track saved at the very start has no seek to wait for, so it must not sit
    /// waiting for one that will never be answered.
    #[test]
    fn a_track_restored_at_the_start_needs_no_seek_to_begin() {
        let lib = Library::new("session-from-zero");
        let mut app = lib.app();
        let track = lib.0.join("Alpha").join("01 song.wav");
        app.restore(config::Settings {
            session: config::Session {
                folder: Some(lib.0.join("Alpha")),
                track: Some(track.to_string_lossy().into_owned()),
                position: Duration::ZERO,
                playing: true,
                ..Default::default()
            },
            ..Default::default()
        });
        app.wait_for_tracks();
        assert!(
            !app.audio.is_paused(),
            "waiting on a seek it never asked for"
        );
    }

    /// A remembered path from some other library must not override the folder this
    /// run was actually pointed at.
    #[test]
    fn a_folder_outside_the_root_is_ignored() {
        let lib = Library::new("session-foreign");
        let mut app = lib.app();
        app.restore(config::Settings {
            session: config::Session {
                folder: Some(PathBuf::from("/somewhere/else")),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(app.cwd, lib.0, "walked out of the root");
        assert!(!app.can_leave());
    }

    /// Libraries change between runs. A folder that has gone stops the walk at the
    /// deepest point that is still there rather than failing the whole restore.
    #[test]
    fn a_deleted_folder_restores_as_far_as_it_can() {
        let lib = Library::new("session-deleted");
        let mut app = lib.app();
        app.restore(config::Settings {
            session: config::Session {
                folder: Some(lib.0.join("Artist").join("Gone")),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(app.cwd, lib.0.join("Artist"), "{}", app.status);
    }

    /// Which source was showing comes back, and the feed that was picked with it.
    #[test]
    fn the_tab_and_feed_come_back() {
        let lib = Library::new("session-tab");
        let mut app = lib.app();
        app.feeds = vec![
            Feed {
                name: "first".into(),
                url: "https://example.com/a.xml".into(),
            },
            Feed {
                name: "second".into(),
                url: TEST_FEED.into(),
            },
        ];
        app.restore(config::Settings {
            session: config::Session {
                tab: Some("feeds".into()),
                feed: Some(TEST_FEED.into()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(app.tab, Tab::Feeds);
        assert_eq!(app.selected_feed().map(|f| f.url.as_str()), Some(TEST_FEED));
    }

    /// A feed that was removed from feeds.txt between runs leaves the cursor where
    /// it was rather than pointing at nothing.
    #[test]
    fn a_feed_that_is_gone_leaves_the_cursor_alone() {
        let lib = Library::new("session-feed-gone");
        let mut app = lib.app();
        let first = app.feeds.first().map(|f| f.url.clone());
        app.restore(config::Settings {
            session: config::Session {
                tab: Some("feeds".into()),
                feed: Some("https://example.com/deleted.xml".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(app.selected_feed().map(|f| f.url.clone()), first);
    }

    /// The resume is spent by the first listing, found or not. Otherwise browsing
    /// into that folder an hour later would start playing on its own.
    #[test]
    fn the_resume_does_not_lie_in_wait() {
        let lib = Library::new("session-one-shot");
        let mut app = lib.app();
        // Point it at a track that is not in the folder the app opens on.
        let elsewhere = lib.0.join("Beta").join("01 song.wav");
        app.restore(config::Settings {
            session: config::Session {
                track: Some(elsewhere.to_string_lossy().into_owned()),
                position: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        app.wait_for_tracks();
        assert!(!app.is_playing_something(), "resumed the wrong folder");

        // Now browse to where that track actually is: still nothing must start.
        app.select_folder(folder_row(&app, "Beta"));
        app.wait_for_tracks();
        assert!(
            !app.is_playing_something(),
            "browsing started playback by itself: {}",
            app.status
        );
    }

    /// A position past the end of the track — re-encoded shorter, or simply
    /// finished last time — starts it again instead of seeking off the end.
    #[test]
    fn a_position_past_the_end_starts_the_track_over() {
        let lib = Library::new("session-past-end");
        let mut app = lib.app();
        let track = lib.0.join("Alpha").join("01 song.wav");
        app.restore(config::Settings {
            session: config::Session {
                folder: Some(lib.0.join("Alpha")),
                track: Some(track.to_string_lossy().into_owned()),
                // The fixture tracks are 2s long.
                position: Duration::from_secs(600),
                ..Default::default()
            },
            ..Default::default()
        });
        app.wait_for_tracks();
        assert!(app.is_playing_something(), "{}", app.status);
        assert_eq!(app.audio.wait_for_seek(), None);
        assert!(
            app.audio.position() < Duration::from_millis(500),
            "seeked past the end: {:?}",
            app.audio.position()
        );
    }

    /// Nothing saved is the first run, which must look like an ordinary start.
    #[test]
    fn an_empty_session_changes_nothing() {
        let lib = Library::new("session-empty");
        let mut app = lib.app();
        app.restore(config::Settings::default());
        app.wait_for_tracks();
        assert_eq!(app.cwd, lib.0);
        assert_eq!(app.tab, Tab::Local);
        assert!(!app.shuffle);
        assert!(!app.is_playing_something());
    }

    /// Shuffle is part of the session too.
    #[test]
    fn shuffle_survives_a_restart() {
        let lib = Library::new("session-shuffle");
        let file =
            std::env::temp_dir().join(format!("tuneterm-sess-sh-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        let mut before = lib.app();
        before.settings_file = Some(file.clone());
        before.toggle_shuffle();
        before.save_settings(true);

        let mut after = lib.app();
        after.restore(config::load_settings_from(&file));
        assert!(after.shuffle);
        let _ = std::fs::remove_file(&file);
    }

    /// Quitting long after the last thing you touched must still record where the
    /// playhead got to. The playhead moves on its own, so "nothing has changed
    /// since the last write" is never true while a track is playing.
    #[test]
    fn quitting_records_where_the_playhead_actually_got_to() {
        let lib = Library::new("session-late-quit");
        let file = std::env::temp_dir().join(format!("tuneterm-sess-q-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        let mut app = lib.app();
        app.settings_file = Some(file.clone());
        app.play_index(0);

        // The write the start of the track earned, which lands near 0:00.
        app.save_settings(true);
        let early = config::load_settings_from(&file).session.position;

        // Now play on without touching anything, the way listening works.
        app.seek_to(0.5);
        settled(&app);
        let now = app.audio.position();
        assert!(
            now > early + Duration::from_millis(400),
            "the playhead did not move"
        );

        app.save_settings(true);
        let saved = config::load_settings_from(&file).session.position;
        assert!(
            saved.abs_diff(now) < Duration::from_millis(400),
            "quit recorded {saved:?}, but the playhead was at {now:?}"
        );

        let _ = std::fs::remove_file(&file);
    }

    /// The playhead is the one part of the session that changes without anyone
    /// asking, so it is the one part `tick` has to notice by itself — otherwise a
    /// kill, a crash or a closed terminal loses everything since the last keypress.
    #[test]
    fn playing_on_its_own_earns_a_write() {
        let lib = Library::new("session-tick");
        let file = std::env::temp_dir().join(format!("tuneterm-sess-t-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        let mut app = lib.app();
        app.settings_file = Some(file.clone());
        app.play_index(0);
        app.save_settings(true);

        // Somewhere past the once-a-second mark, without a single keystroke.
        app.seek_to(0.6);
        settled(&app);
        app.tick();
        let saved = config::load_settings_from(&file).session.position;
        assert!(
            saved >= Duration::from_secs(1),
            "a moving playhead owes a write and nobody noticed: disk says {saved:?}"
        );

        let _ = std::fs::remove_file(&file);
    }

    /// The whole thing, driven the way `run` drives it: restore from a real file,
    /// then do nothing but turn the loop. The position on disk has to follow the
    /// playhead, because that is what is left behind when the terminal is closed
    /// or the process is killed.
    #[test]
    fn turning_the_loop_keeps_the_position_on_disk_up_to_date() {
        let lib = Library::new("session-loop");
        let file = std::env::temp_dir().join(format!("tuneterm-sess-l-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);

        let track = lib.0.join("Alpha").join("01 song.wav");
        config::save_settings_to(
            &file,
            &config::Settings {
                session: config::Session {
                    folder: Some(lib.0.join("Alpha")),
                    track: Some(track.to_string_lossy().into_owned()),
                    position: Duration::ZERO,
                    playing: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("seed");

        // Restored the way `main` does it: straight after `new`, with the root
        // scan still in flight.
        let mut app = lib.raw_app();
        app.settings_file = Some(file.clone());
        app.restore(config::load_settings_from(&file));

        // Exactly what `run` calls, for most of a 2s track.
        let deadline = Instant::now() + Duration::from_millis(1800);
        while Instant::now() < deadline {
            app.poll_tracks();
            app.poll_seek();
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(app.is_playing_something(), "never started: {}", app.status);
        assert!(!app.audio.is_paused(), "never let go: {}", app.status);

        let on_disk = config::load_settings_from(&file).session.position;
        assert!(
            on_disk >= Duration::from_secs(1),
            "the loop ran through 1.8s of playback but disk still says {on_disk:?}"
        );

        let _ = std::fs::remove_file(&file);
    }

    /// Nothing owed, nothing written — the loop calls this every pass, and a
    /// hundred writes a second is not a way to keep a file.
    #[test]
    fn an_idle_pass_writes_nothing() {
        let lib = Library::new("session-quiet");
        let mut app = lib.app();
        let file =
            std::env::temp_dir().join(format!("tuneterm-vol-quiet-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        app.settings_file = Some(file.clone());

        app.tick();
        app.save_settings(false);
        assert!(!file.exists(), "wrote a file with nothing to say");

        // Quitting is not an idle pass: it records the session whether or not
        // anything announced itself, because the playhead never does.
        app.save_settings(true);
        assert!(file.exists(), "quitting recorded nothing");
        let _ = std::fs::remove_file(&file);
    }

    /// Shuffle changes the order the queue is played in, and nothing else: every
    /// track still comes up, exactly once, until the queue runs out.
    #[test]
    fn shuffle_plays_the_whole_queue_exactly_once() {
        let lib = Library::new("shuffle-cover");
        let mut app = lib.app();
        app.select_folder(2); // Artist, four tracks under Late plus two under Early
        app.wait_for_tracks();
        let total = app.tracks.len();
        assert!(total >= 4, "need a few tracks to shuffle, got {total}");

        app.toggle_shuffle();
        app.play_index(0);

        let mut heard = vec![app.queue_pos.expect("playing")];
        for _ in 1..total {
            app.next_track();
            heard.push(app.queue_pos.expect("still playing"));
        }
        // Used up, so on into the next folder rather than round this one again.
        app.next_track();
        assert_eq!(app.following_into, Some(lib.0.join("Beta")));

        let mut seen = heard.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            total,
            "a track was repeated or skipped: {heard:?}"
        );
        assert_eq!(heard[0], 0, "the track you started stays the one playing");
    }

    /// With a server, `..` at the root takes in its folders too — each folder once,
    /// the local one where both have it, as in the folder list — and what was
    /// already listed is not asked for again.
    #[test]
    fn the_whole_library_takes_in_the_server() {
        let lib = Library::new("whole-served");
        let served = crate::server::TempDir::new("whole-served-srv");
        let wav = silent_wav(2);
        for (album, name) in [("Gamma", "01 far.wav"), ("Alpha", "01 there.wav")] {
            let dir = served.0.join(album);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), &wav).unwrap();
        }
        let addr = crate::server::spawn_for_test(&served.0, Some("k"));

        let mut app = lib.app();
        app.settings_file = Some(lib.0.join("settings.txt"));
        app.open_add_server();
        for c in format!("k@{addr}").chars() {
            app.prompt_key(c);
        }
        app.submit_prompt();
        app.wait_for_tracks();
        assert!(
            app.memo.contains_key(&lib.0.join("Alpha")),
            "Alpha was listed"
        );

        app.select_folder(0);
        app.wait_for_tracks();
        let far = PathBuf::from(format!("tuneterm://{addr}/Gamma/01 far.wav"));
        let there = PathBuf::from(format!("tuneterm://{addr}/Alpha/01 there.wav"));
        assert_eq!(app.tracks.len(), 12, "eleven here and Gamma's one");
        assert_eq!(
            app.tracks.last().map(|t| &t.path),
            Some(&far),
            "in folder order"
        );
        assert!(
            !app.tracks.iter().any(|t| t.path == there),
            "Alpha is the local one"
        );
        assert!(
            app.memo.contains_key(&lib.0.join("Artist")),
            "folders listed on the way are remembered"
        );
    }

    /// An app on `lib` browsing the server at `addr`, with the token `k`.
    fn app_on_server(lib: &Library, addr: std::net::SocketAddr) -> App {
        let mut app = lib.app();
        app.settings_file = Some(lib.0.join("settings.txt"));
        app.open_add_server();
        for c in format!("k@{addr}").chars() {
            app.prompt_key(c);
        }
        app.submit_prompt();
        assert!(
            app.prompt.is_none(),
            "{:?}",
            app.prompt.as_ref().map(|p| &p.hint)
        );
        app.wait_for_tracks();
        app
    }

    /// Stars live on the server: given by one player, clicked under the cover or
    /// in the list, and seen by another — even one that remembered the folder from
    /// before they were given.
    #[test]
    fn stars_are_kept_on_the_server_and_seen_by_every_player() {
        let served = crate::server::TempDir::new("stars-served");
        let dir = served.0.join("Gamma");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01 far.wav"), silent_wav(2)).unwrap();
        std::fs::write(dir.join("02 near.wav"), silent_wav(2)).unwrap();
        let addr = crate::server::spawn_for_test(&served.0, Some("k"));

        let one = Library::new("stars-one");
        let two = Library::new("stars-two");
        let mut first = app_on_server(&one, addr);
        let mut second = app_on_server(&two, addr);

        // The second player reads Gamma before any stars exist, and remembers it.
        second.select_folder(folder_row(&second, "Gamma"));
        second.wait_for_tracks();
        assert_eq!(second.tracks[0].stars, None);

        // Two stars for the playing track, clicked under the cover.
        first.select_folder(folder_row(&first, "Gamma"));
        first.wait_for_tracks();
        first.play_index(0);
        wait_for_open(&mut first);
        first.star_areas = [
            Rect::new(10, 20, 2, 1),
            Rect::new(12, 20, 2, 1),
            Rect::new(14, 20, 2, 1),
        ];
        first.click(Position { x: 13, y: 20 }, Instant::now());
        assert_eq!(
            first.now_playing().and_then(|t| t.stars),
            Some(2),
            "{}",
            first.status
        );
        assert_eq!(first.tracks[0].stars, Some(2), "the list agrees");

        // Three for the second track, clicked in its row of the list.
        first.track_rows = rows(30, 1, 10);
        first.track_star_x = Some(35);
        first.click(Position { x: 37, y: 2 }, Instant::now());
        assert_eq!(first.tracks[1].stars, Some(3), "{}", first.status);

        // The second player browses away and back: the folder changed, so it is
        // read again rather than served from memory.
        second.select_folder(folder_row(&second, "Alpha"));
        second.wait_for_tracks();
        second.relist_root();
        second.select_folder(folder_row(&second, "Gamma"));
        second.wait_for_tracks();
        let stars: Vec<_> = second.tracks.iter().map(|t| t.stars).collect();
        assert_eq!(stars, [Some(2), Some(3)]);

        // The same star again takes them away.
        first.click(Position { x: 13, y: 20 }, Instant::now());
        assert_eq!(first.now_playing().and_then(|t| t.stars), None);
    }

    /// A local track has no stars to give: only a server keeps them.
    #[test]
    fn a_local_track_takes_no_stars() {
        let lib = Library::new("stars-local");
        let mut app = lib.app();
        app.wait_for_tracks();
        app.play_index(0);
        app.cycle_rating();
        assert_eq!(app.now_playing().and_then(|t| t.stars), None);
        assert_eq!(app.status, "stars are for tracks on the server");
    }

    /// Block until the folder playback is running on into has been listed.
    fn wait_for_follow(app: &mut App) {
        for _ in 0..400 {
            app.poll_follow();
            if app.following_into.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the next folder was never listed");
    }

    /// `..` at the root is the whole library: every folder, and every level down.
    #[test]
    fn the_up_row_at_the_root_lists_everything() {
        let lib = Library::new("root-up");
        let mut app = lib.app();
        app.select_folder(0);
        app.wait_for_tracks();
        assert_eq!(app.listing_dir(), lib.0);
        assert_eq!(app.tracks.len(), 11, "Alpha, both albums of Artist, Beta");

        // Enter there has nowhere to climb to, so it goes to the tracks.
        app.enter_folder();
        assert_eq!(app.cwd, lib.0);
        assert_eq!(app.focus, Pane::Tracks);
    }

    /// The end of a folder is not the end of the music: it goes on into the one
    /// listed after it, and the cursor follows it there.
    #[test]
    fn a_finished_folder_runs_on_into_the_next() {
        let lib = Library::new("run-on");
        let mut app = lib.app(); // on Alpha, three tracks
        app.wait_for_tracks();
        app.play_index(2);
        app.next_track();
        wait_for_follow(&mut app);

        let playing = app.now_playing().expect("stopped at the end of Alpha");
        assert!(
            playing.path.starts_with(lib.0.join("Artist").join("Early")),
            "{:?}",
            playing.path
        );
        assert_eq!(
            app.selected_folder().map(|f| f.label.as_str()),
            Some("Artist")
        );
    }

    /// Out of an album, on into the artist's next album; out of the artist's last,
    /// on into what follows the artist; and past the last folder, a stop.
    #[test]
    fn running_on_climbs_out_and_stops_at_the_end() {
        let lib = Library::new("run-on-climb");
        let mut app = lib.app();
        app.select_folder(2); // Artist/
        app.wait_for_tracks();
        app.enter_folder(); // on Early
        app.wait_for_tracks();

        app.play_index(1); // Early's last
        app.next_track();
        wait_for_follow(&mut app);
        let playing = app.now_playing().expect("stopped after Early").path.clone();
        assert!(
            playing.starts_with(lib.0.join("Artist").join("Late")),
            "{playing:?}"
        );

        app.queue_pos = Some(app.queue.len() - 1); // Late's last
        app.next_track();
        wait_for_follow(&mut app);
        let playing = app
            .now_playing()
            .expect("stopped after Artist")
            .path
            .clone();
        assert!(playing.starts_with(lib.0.join("Beta")), "{playing:?}");

        app.queue_pos = Some(app.queue.len() - 1); // Beta's last, and the library's
        app.next_track();
        assert!(!app.is_playing_something());
        assert_eq!(app.status, "end of queue");
    }

    /// The coverage test above would still pass if the "permutation" came back in
    /// listing order. Two hundred entries settles that: in order means unshuffled.
    #[test]
    fn the_shuffle_actually_shuffles() {
        const N: usize = 200;
        let lib = Library::new("shuffle-order");
        let mut app = lib.app();
        app.queue = vec![app.tracks[0].clone(); N];
        app.reshuffle_from(None);

        let order = app.shuffle_order.clone();
        assert_ne!(
            order,
            (0..N).collect::<Vec<_>>(),
            "the listing order came back untouched"
        );
        let mut sorted = order;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..N).collect::<Vec<_>>(), "not a permutation");
    }

    /// Previous has to undo next, or the button is a lie.
    #[test]
    fn shuffle_walks_backwards_the_way_it_came() {
        let lib = Library::new("shuffle-back");
        let mut app = lib.app();
        app.toggle_shuffle();
        app.play_index(0);

        app.next_track();
        let second = app.queue_pos.expect("advanced");
        app.next_track();
        app.prev_track();
        assert_eq!(app.queue_pos, Some(second));
        app.prev_track();
        assert_eq!(app.queue_pos, Some(0), "back to where it started");
    }

    /// Turning it on mid-track reorders what is coming, not what is playing.
    #[test]
    fn turning_shuffle_on_does_not_interrupt_the_track() {
        let lib = Library::new("shuffle-live");
        let mut app = lib.app();
        app.play_index(1);
        let playing = app.now_playing().map(|t| t.path.clone());

        app.toggle_shuffle();
        assert!(app.shuffle);
        assert_eq!(app.now_playing().map(|t| t.path.clone()), playing);
        assert_eq!(app.queue_pos, Some(1));

        // And off again leaves the listing order intact.
        app.toggle_shuffle();
        assert!(!app.shuffle);
        app.next_track();
        assert_eq!(app.queue_pos, Some(2));
    }

    /// The button in the key bar is the only sign shuffling is on, so it has to be
    /// clickable and it has to be where the renderer said it was.
    #[test]
    fn clicking_the_shuffle_button_toggles_it() {
        let lib = Library::new("shuffle-click");
        let mut app = lib.app();
        app.shuffle_area = Rect::new(40, 31, 12, 1);

        app.click(Position { x: 45, y: 31 }, Instant::now());
        assert!(app.shuffle, "{}", app.status);
        app.click(Position { x: 45, y: 31 }, Instant::now());
        assert!(!app.shuffle, "{}", app.status);
    }

    /// Seeking with nothing playing must be a no-op, not a panic.
    #[test]
    fn seeking_without_playback_is_harmless() {
        let lib = Library::new("seek-idle");
        let mut app = lib.app();
        app.seek_bar = Rect::new(0, 30, 11, 1);
        app.click(Position { x: 5, y: 30 }, Instant::now());
        app.seek_by(10);
        assert!(!app.is_playing_something());
    }

    /// The transport buttons must be distinguishable by position alone.
    #[test]
    fn transport_buttons_route_by_area() {
        let lib = Library::new("transport");
        let mut app = lib.app();
        app.prev_area = Rect::new(0, 20, 7, 3);
        app.play_area = Rect::new(7, 20, 10, 3);
        app.next_area = Rect::new(17, 20, 7, 3);
        app.track_rows = Rect::new(30, 1, 10, 10);

        // Next with nothing playing starts at the top of the folder.
        app.click(Position { x: 20, y: 21 }, Instant::now());
        assert_eq!(app.playing_row(), Some(0), "next started playback");

        app.click(Position { x: 20, y: 21 }, Instant::now());
        assert_eq!(app.playing_row(), Some(1), "next advanced");

        app.click(Position { x: 3, y: 21 }, Instant::now());
        assert_eq!(app.playing_row(), Some(0), "prev went back");

        // Clicking a transport button must never touch the selection.
        assert_eq!(app.focus, Pane::Folders, "focus unchanged by transport");
    }

    /// Switching a track must not block on the cover — that was a ~600 ms stall on
    /// a 1400 px cover in a debug build.
    #[test]
    fn playing_does_not_wait_for_the_cover() {
        let lib = Library::new("cover-async");
        let mut app = lib.app();
        app.art_budget = Rect::new(0, 0, 30, 15);

        app.play_index(0);

        assert!(
            app.is_playing_something(),
            "playback started: {}",
            app.status
        );
        assert!(app.cover_pending, "cover handed to the worker, not awaited");
        assert!(app.cover.is_none(), "nothing drawn yet");
    }

    /// Rapid switching must leave exactly one outstanding request, and the reply
    /// that finally lands must belong to the track that is actually playing.
    #[test]
    fn rapid_switching_keeps_one_request_and_the_right_answer() {
        let lib = Library::new("cover-race");
        let mut app = lib.app();
        app.art_budget = Rect::new(0, 0, 30, 15);

        for _ in 0..12 {
            app.next_track();
        }
        let landed_on = app.playing_row();

        // Let the worker settle, then take whatever it produced.
        for _ in 0..200 {
            app.poll_cover();
            if !app.cover_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            app.playing_row(),
            landed_on,
            "playback did not move on its own"
        );
        assert!(!app.cover_pending, "worker never answered");
        // The fixture has no artwork, so `None` is the correct answer — the point is
        // that we got exactly one settled answer rather than a backlog.
        assert!(app.cover.is_none());
        app.poll_cover();
        assert!(!app.cover_pending, "a stale reply revived the pending flag");
    }

    /// The worker scales to fill the pane exactly, so both growing and shrinking
    /// need a fresh pass — otherwise the render thread ends up resizing. A nudge
    /// smaller than the slack must be ignored, or every column of a drag restarts
    /// the work.
    #[test]
    fn a_resized_pane_refetches_but_a_nudge_does_not() {
        let lib = Library::new("cover-resize");
        let mut app = lib.app();
        app.art_budget = Rect::new(0, 0, 30, 15);
        app.play_index(0);
        while app.cover_pending {
            app.poll_cover();
            std::thread::sleep(Duration::from_millis(5));
        }

        // One column at 10 px per cell is inside the 16 px slack.
        app.art_budget = Rect::new(0, 0, 31, 15);
        app.refresh_cover_for_resize();
        assert!(!app.cover_pending, "a one-column nudge must not refetch");

        app.art_budget = Rect::new(0, 0, 10, 5);
        app.refresh_cover_for_resize();
        assert!(app.cover_pending, "shrinking must refetch");

        while app.cover_pending {
            app.poll_cover();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.art_budget = Rect::new(0, 0, 90, 45);
        app.refresh_cover_for_resize();
        assert!(app.cover_pending, "growing must refetch");
    }

    /// The reported symptom: after spinning the wheel hard, reversing had to work
    /// off a backlog before it moved. Clamping must leave nothing owed, so three
    /// events back is exactly three rows back — no matter how far past the end the
    /// burst went.
    #[test]
    fn a_scroll_burst_leaves_nothing_owed() {
        let lib = Library::new("scroll-burst");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);
        // The server button is the last row at the root.
        let last = app.folder_row_count() - 1;

        for _ in 0..200 {
            app.scroll(Position { x: 2, y: 2 }, 1);
        }
        assert_eq!(app.folder_state.selected(), Some(last), "pinned to the end");

        app.scroll(Position { x: 2, y: 2 }, -1);
        assert_eq!(
            app.folder_state.selected(),
            Some(last - 1),
            "one event back must be one row back"
        );
    }

    #[test]
    fn a_track_scroll_burst_leaves_nothing_owed() {
        let lib = Library::new("scroll-tracks");
        let mut app = lib.app();
        app.track_rows = rows(30, 1, 10);
        let last = app.tracks.len() - 1;

        for _ in 0..200 {
            app.scroll(Position { x: 32, y: 2 }, 1);
        }
        assert_eq!(app.track_state.selected(), Some(last));

        for _ in 0..2 {
            app.scroll(Position { x: 32, y: 2 }, -1);
        }
        assert_eq!(app.track_state.selected(), Some(last - 2));
    }

    /// Scrolling an empty list must not panic on `clamp(0, -1)`.
    #[test]
    fn scrolling_an_empty_list_is_harmless() {
        let dir = std::env::temp_dir().join(format!("tuneterm-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(
            dir.clone(),
            Picker::halfblocks(),
            media::Bridge::detached(),
            Wake::none(),
        )
        .expect("app init");
        app.folder_rows = rows(0, 1, 10);
        app.track_rows = rows(30, 1, 10);

        app.scroll(Position { x: 2, y: 2 }, 1);
        app.scroll(Position { x: 32, y: 2 }, -1);
        assert!(app.folders.is_empty() && app.tracks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clicking a tab switches source and nothing else.
    #[test]
    fn clicking_a_tab_switches_source() {
        let lib = Library::new("tabs");
        let mut app = lib.app();
        // One strip per tab, laid out as the renderer would.
        let mut x = 1;
        for (index, tab) in Tab::ALL.iter().enumerate() {
            let width = tab.label().chars().count() as u16 + 2;
            app.tab_areas[index] = Rect::new(x, 0, width, 1);
            x += width + 1;
        }

        assert_eq!(app.tab, Tab::Local, "local by default");
        for (index, tab) in Tab::ALL.iter().enumerate() {
            let area = app.tab_areas[index];
            app.click(
                Position {
                    x: area.x + 1,
                    y: 0,
                },
                Instant::now(),
            );
            assert_eq!(app.tab, *tab, "clicking {:?}", tab);
        }
    }

    /// Switching source must not interrupt playback: the queue outlives the panes.
    #[test]
    fn switching_tabs_keeps_playing() {
        let lib = Library::new("tab-play");
        let mut app = lib.app();
        app.play_index(0);
        let playing = app.now_playing().map(|t| t.path.clone());
        assert!(playing.is_some(), "playback failed: {}", app.status);

        for tab in Tab::ALL {
            app.select_tab(tab);
            assert_eq!(
                app.now_playing().map(|t| t.path.clone()),
                playing,
                "playback survives {tab:?}"
            );
            assert!(app.is_playing_something(), "{tab:?}");
        }
    }

    /// The panes are gone on another tab, so their recorded rects must be too —
    /// otherwise a click in that space would still select a hidden row.
    #[test]
    fn a_hidden_pane_takes_no_clicks() {
        let lib = Library::new("tab-hidden");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);
        app.track_rows = rows(30, 1, 10);
        let before = app.folder_state.selected();

        // What `draw` does when the local panes are not on screen.
        app.folder_rows = Rect::ZERO;
        app.track_rows = Rect::ZERO;
        app.click(Position { x: 2, y: 3 }, Instant::now());
        app.click(Position { x: 32, y: 3 }, Instant::now());

        assert_eq!(app.folder_state.selected(), before, "selection untouched");
        assert!(!app.is_playing_something());
    }

    /// Unroutable on purpose: these tests are about the list and the input, not about
    /// fetching, and they must not depend on a network or on someone else's server.
    const TEST_FEED: &str = "https://tuneterm.invalid/feed.xml";

    /// A feed list that writes somewhere harmless and fetches nothing real.
    fn feeds_app(lib: &Library, name: &str) -> App {
        let mut app = lib.app();
        let file =
            std::env::temp_dir().join(format!("tuneterm-feeds-{}-{name}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        app.feeds_file = Some(file);
        app.feeds = vec![Feed {
            name: "test".into(),
            url: TEST_FEED.into(),
        }];
        app.feed_state.select(Some(0));
        app.select_tab(Tab::Feeds);
        app
    }

    /// The button opens the field, typing fills it, Enter stores it — and it lands on
    /// disk, since a list that vanishes on exit is no list.
    #[test]
    fn adding_a_feed_through_the_prompt_persists_it() {
        let lib = Library::new("feed-add");
        let mut app = feeds_app(&lib, "add");
        app.add_area = Rect::new(0, 4, 18, 1);

        app.click(Position { x: 3, y: 4 }, Instant::now());
        assert!(app.prompt.is_some(), "the button opens the field");

        for ch in "https://example.com/p.xml".chars() {
            assert!(app.prompt_key(ch), "keys go to the field");
        }
        app.submit_prompt();

        assert!(app.prompt.is_none(), "closed on success");
        assert_eq!(app.feeds.len(), 2);
        assert_eq!(app.feeds[1].url, "https://example.com/p.xml");
        assert_eq!(app.feeds[1].name, "example.com", "named after its host");
        assert_eq!(
            app.feed_state.selected(),
            Some(1),
            "cursor follows the new entry"
        );

        let saved = config::load_feeds_from(app.feeds_file.as_ref().unwrap());
        assert_eq!(saved, app.feeds, "written to disk");
    }

    /// Refusing input must not throw away what was pasted.
    #[test]
    fn a_bad_url_keeps_the_field_open_with_a_reason() {
        let lib = Library::new("feed-bad");
        let mut app = feeds_app(&lib, "bad");
        app.open_add_feed();
        for ch in "example.com/p.xml".chars() {
            app.prompt_key(ch);
        }
        app.submit_prompt();

        let prompt = app.prompt.as_ref().expect("still open");
        assert!(prompt.hint.contains("http"), "says why: {}", prompt.hint);
        assert_eq!(prompt.input, "example.com/p.xml", "input kept");
        assert_eq!(app.feeds.len(), 1, "nothing added");
    }

    #[test]
    fn a_duplicate_is_refused_without_losing_the_list() {
        let lib = Library::new("feed-dup");
        let mut app = feeds_app(&lib, "dup");
        app.open_add_feed();
        for ch in TEST_FEED.chars() {
            app.prompt_key(ch);
        }
        app.submit_prompt();

        assert!(app.prompt.as_ref().unwrap().hint.contains("already"));
        assert_eq!(app.feeds.len(), 1);
    }

    #[test]
    fn escape_cancels_without_adding() {
        let lib = Library::new("feed-esc");
        let mut app = feeds_app(&lib, "esc");
        app.open_add_feed();
        for ch in "https://example.com/x.xml".chars() {
            app.prompt_key(ch);
        }
        app.cancel_prompt();
        assert!(app.prompt.is_none());
        assert_eq!(app.feeds.len(), 1);
    }

    /// An empty field on Enter just closes: nothing typed, nothing meant.
    #[test]
    fn submitting_nothing_closes_the_field() {
        let lib = Library::new("feed-empty");
        let mut app = feeds_app(&lib, "empty");
        app.open_add_feed();
        app.submit_prompt();
        assert!(app.prompt.is_none());
        assert_eq!(app.feeds.len(), 1);
    }

    #[test]
    fn backspace_edits_the_field() {
        let lib = Library::new("feed-bs");
        let mut app = feeds_app(&lib, "bs");
        app.open_add_feed();
        for ch in "abc".chars() {
            app.prompt_key(ch);
        }
        app.prompt_backspace();
        assert_eq!(app.prompt.as_ref().unwrap().input, "ab");
        app.prompt_backspace();
        app.prompt_backspace();
        app.prompt_backspace(); // one too many
        assert_eq!(app.prompt.as_ref().unwrap().input, "");
    }

    /// While the field is open the app behind it must be inert, or a click meant for
    /// the box would select a row underneath.
    #[test]
    fn an_open_field_swallows_clicks() {
        let lib = Library::new("feed-modal");
        let mut app = feeds_app(&lib, "modal");
        app.tab_areas[0] = Rect::new(1, 0, 7, 1);
        app.open_add_feed();

        app.click(Position { x: 3, y: 0 }, Instant::now());
        assert_eq!(app.tab, Tab::Feeds, "the tab click did not go through");
        assert!(app.prompt.is_some(), "and the field is still open");
    }

    #[test]
    fn removing_a_feed_persists_and_moves_the_cursor() {
        let lib = Library::new("feed-del");
        let mut app = feeds_app(&lib, "del");
        app.feeds.push(Feed {
            name: "example.com".into(),
            url: "https://example.com/p.xml".into(),
        });
        app.feed_state.select(Some(1));

        app.remove_selected_feed();
        assert_eq!(app.feeds.len(), 1);
        assert_eq!(app.feed_state.selected(), Some(0), "cursor stayed in range");
        assert_eq!(
            config::load_feeds_from(app.feeds_file.as_ref().unwrap()),
            app.feeds
        );

        app.remove_selected_feed();
        assert!(app.feeds.is_empty());
        assert_eq!(app.feed_state.selected(), None, "nothing left to point at");
    }

    /// Regression: keys were routed by tab rather than by focus, so the arrows always
    /// moved the feed list and the episodes were unreachable.
    #[test]
    fn focus_decides_which_list_the_arrows_move() {
        let lib = Library::new("feed-focus");
        let mut app = feeds_app(&lib, "focus");
        // Two feeds and a stand-in episode list.
        app.feeds.push(Feed {
            name: "example.com".into(),
            url: "https://example.com/p.xml".into(),
        });
        app.feed_state.select(Some(0));

        app.focus = Pane::Folders;
        app.move_selection(1);
        assert_eq!(app.feed_state.selected(), Some(1), "feed list moved");

        // Changing feed clears the episode list, so stand one in afterwards.
        app.tracks = lib.app().tracks;
        assert!(app.tracks.len() >= 3, "need episodes to move through");
        app.track_state.select(Some(0));

        app.focus = Pane::Tracks;
        app.move_selection(1);
        assert_eq!(app.track_state.selected(), Some(1), "episodes moved");
        assert_eq!(app.feed_state.selected(), Some(1), "feed list untouched");

        // And back: the arrows follow focus, not the tab.
        app.focus = Pane::Folders;
        app.move_selection(-1);
        assert_eq!(app.feed_state.selected(), Some(0));
    }

    /// Enter in the left pane of Feeds hands over to the episodes.
    #[test]
    fn enter_moves_from_the_feed_to_its_episodes() {
        let lib = Library::new("feed-enter");
        let mut app = feeds_app(&lib, "enter");
        app.focus = Pane::Folders;
        app.enter_selected();
        assert_eq!(app.focus, Pane::Tracks);
    }

    /// A click in either pane takes focus, so the arrows follow the mouse.
    #[test]
    fn clicking_a_feed_row_takes_focus() {
        let lib = Library::new("feed-click");
        let mut app = feeds_app(&lib, "click");
        app.feed_rows = rows(0, 1, 10);
        app.track_rows = rows(30, 1, 10);
        app.focus = Pane::Tracks;

        app.click(Position { x: 2, y: 1 }, Instant::now());
        assert_eq!(app.focus, Pane::Folders);
    }

    /// A feed lists newest first; the interface shows the archive in reading order.
    #[test]
    fn episodes_are_listed_oldest_first() {
        let channel = crate::feed::Channel {
            title: "Archive".into(),
            episodes: vec![
                crate::feed::Episode {
                    title: "Episode 3".into(),
                    url: "https://example.com/3.mp3".into(),
                    ..Default::default()
                },
                crate::feed::Episode {
                    title: "Episode 2".into(),
                    url: "https://example.com/2.mp3".into(),
                    ..Default::default()
                },
                crate::feed::Episode {
                    title: "Episode 1".into(),
                    url: "https://example.com/1.mp3".into(),
                    ..Default::default()
                },
            ],
        };
        let tracks = library::tracks_from_feed(&channel);
        let titles: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Episode 1", "Episode 2", "Episode 3"]);
    }

    /// A feed added by URL should end up labelled by its channel, not its host.
    #[test]
    fn a_fetched_feed_takes_its_channel_name() {
        let lib = Library::new("feed-name");
        let mut app = feeds_app(&lib, "name");
        app.feeds[0].name = config::host_of(TEST_FEED); // as `+ Add feed` leaves it

        app.adopt_channel_title("Music For Programming");
        assert_eq!(app.feeds[0].name, "Music For Programming");
        assert_eq!(
            config::load_feeds_from(app.feeds_file.as_ref().unwrap())[0].name,
            "Music For Programming",
            "and it is remembered"
        );
    }

    /// A name from `feeds.txt` is the user's, and a feed must not rename itself over it.
    #[test]
    fn a_chosen_name_is_not_overwritten() {
        let lib = Library::new("feed-keep");
        let mut app = feeds_app(&lib, "keep");
        app.feeds[0].name = "My Mixes".into();

        app.adopt_channel_title("Something Else");
        assert_eq!(app.feeds[0].name, "My Mixes");
    }

    /// A click outside every pane must not select or play anything.
    #[test]
    fn click_outside_panes_is_ignored() {
        let lib = Library::new("click-outside");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);
        app.track_rows = rows(30, 1, 10);

        app.click(Position { x: 200, y: 200 }, Instant::now());

        assert_eq!(app.focus, Pane::Folders, "unchanged");
        assert!(!app.is_playing_something());
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use ratatui_image::picker::Picker;
    use std::time::Instant;

    /// `cargo test bench_switch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_switch() {
        use image::{DynamicImage, RgbImage};
        let root = std::env::temp_dir().join(format!("tuneterm-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("Album");
        std::fs::create_dir_all(&dir).unwrap();

        // A 1400px cover: the slow case that used to block the UI.
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(1400, 1400, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 90])
        }));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        img.write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
        std::fs::write(dir.join("cover.jpg"), jpeg.into_inner()).unwrap();
        for i in 1..=4 {
            std::fs::write(dir.join(format!("{i:02}.wav")), super::tests::silent_wav(2)).unwrap();
        }

        let mut app = App::new(
            root.clone(),
            Picker::halfblocks(),
            media::Bridge::detached(),
            Wake::none(),
        )
        .unwrap();
        app.art_budget = Rect::new(0, 0, 30, 15);

        for round in 0..4 {
            let t = Instant::now();
            app.play_index(round);
            let blocked = t.elapsed();
            // Reused from the album already in memory, so no blank frame.
            let instant = app.cover.is_some();

            let t = Instant::now();
            let mut waited = Duration::ZERO;
            while app.cover_pending && waited < Duration::from_secs(5) {
                app.poll_cover();
                std::thread::sleep(Duration::from_millis(5));
                waited = t.elapsed();
            }
            println!(
                "  play_index blocked {:>8.1?}   shown at once: {:<5}   worker replied after {:>8.1?}",
                blocked, instant, waited
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use ratatui_image::picker::Picker;

    /// The whole path against the real feed:
    /// `cargo test plays_a_real_episode -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn plays_a_real_episode() {
        let mut app = App::new(
            std::env::temp_dir().join("tuneterm-live-empty"),
            Picker::halfblocks(),
            media::Bridge::detached(),
            Wake::none(),
        )
        .expect("app");
        app.feeds_file = None; // never write the real config
        app.feeds = vec![config::default_feed()];
        app.feed_state.select(Some(0));

        app.select_tab(Tab::Feeds);
        for _ in 0..300 {
            app.poll_feed();
            if !app.feed_loading {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("status: {}", app.status);
        assert!(!app.tracks.is_empty(), "no episodes: {}", app.status);
        println!("episodes: {}", app.tracks.len());
        let first = &app.tracks[0];
        println!(
            "first: {} — {} [{:?}]",
            first.artist, first.title, first.duration
        );
        assert!(first.url.is_some(), "episode carries no url");

        let start = std::time::Instant::now();
        app.play_index(0);
        println!("play_index blocked {:?}", start.elapsed());
        assert!(app.is_playing_something(), "did not start: {}", app.status);

        std::thread::sleep(Duration::from_millis(500));
        assert!(
            app.audio.position() > Duration::ZERO,
            "the playhead never moved"
        );
        println!("position: {:?}", app.audio.position());

        // Artwork comes off the network for these.
        for _ in 0..200 {
            app.poll_cover();
            if !app.cover_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("cover: {:?}", app.cover_size);
        assert!(app.cover_size.is_some(), "no artwork");
    }
}
