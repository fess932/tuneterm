use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::cover::{self, CoverLoader};
use crate::library::{self, Folder, Track};
use crate::media::{self, Command, NowPlaying};
use crate::player::AudioPlayer;
use crate::worker::Worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Folders,
    Tracks,
}

pub struct App {
    pub root: PathBuf,
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
    /// Directory whose tracks are listed, and whether the worker is still on it.
    tracks_dir: Option<PathBuf>,
    pub tracks_loading: bool,
    scan: Worker<PathBuf, Vec<Track>>,
    scan_generation: u64,
    /// Recently listed directories, so moving back over a folder is instant.
    memo: HashMap<PathBuf, Vec<Track>>,
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

    /// Written during render so mouse events can hit-test. `*_rows` cover only
    /// the data rows of each table — no border, no header — and `seek_bar` only
    /// the gauge itself, not the time label beside it.
    pub play_area: Rect,
    pub prev_area: Rect,
    pub next_area: Rect,
    pub seek_bar: Rect,
    pub folder_rows: Rect,
    pub track_rows: Rect,
    /// Pane, row and time of the last left click, for double-click detection.
    last_click: Option<(Pane, usize, Instant)>,

    /// Media keys and the OS "now playing" panel.
    media: media::Bridge,
    /// Last state handed to the OS, so we only publish on a real change.
    published: Option<NowPlaying>,

    pub status: String,
    pub should_quit: bool,
}

/// Two clicks on the same row within this window count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

impl App {
    pub fn new(root: PathBuf, picker: Picker, media: media::Bridge) -> Result<Self> {
        let folders = library::list_subdirs(&root);
        let mut folder_state = TableState::default();
        if !folders.is_empty() {
            folder_state.select(Some(0));
        }

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
            tracks_dir: None,
            tracks_loading: false,
            scan: Worker::spawn("scan", |dir: PathBuf| library::scan_tracks_deep(&dir)),
            scan_generation: 0,
            memo: HashMap::new(),
            memo_order: VecDeque::new(),
            cover: None,
            picker,
            audio: AudioPlayer::new()?,
            cover_size: None,
            cover_pending: false,
            cover_file: None,
            cover_memo: None,
            cover_loader: CoverLoader::new(),
            cover_generation: 0,
            cover_requested_box: (0, 0),
            art_budget: Rect::ZERO,
            play_area: Rect::ZERO,
            prev_area: Rect::ZERO,
            next_area: Rect::ZERO,
            seek_bar: Rect::ZERO,
            folder_rows: Rect::ZERO,
            track_rows: Rect::ZERO,
            last_click: None,
            media,
            published: None,
            status: String::new(),
            should_quit: false,
        };
        app.status = if app.folders.is_empty() && library::scan_tracks(&app.root).is_empty() {
            format!("no audio found under {}", app.root.display())
        } else {
            format!("{} folders", app.folders.len())
        };
        app.reload_tracks();
        Ok(app)
    }

    pub fn selected_folder(&self) -> Option<&Folder> {
        self.folders.get(self.folder_state.selected()?)
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

        if let Some(cached) = self.memo.get(&dir) {
            let tracks = cached.clone();
            self.show_tracks(tracks);
            return;
        }
        self.tracks_loading = true;
        self.scan_generation += 1;
        self.scan.request(self.scan_generation, dir);
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
        for (generation, tracks) in self.scan.drain() {
            if generation == self.scan_generation {
                newest = Some(tracks);
            }
        }
        let Some(tracks) = newest else {
            return;
        };
        self.tracks_loading = false;
        if let Some(dir) = self.tracks_dir.clone() {
            self.remember(dir, tracks.clone());
        }
        self.show_tracks(tracks);
    }

    fn show_tracks(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.track_state = TableState::default();
        if !self.tracks.is_empty() {
            self.track_state.select(Some(0));
        }
    }

    /// Bounded so browsing a large tree cannot grow without limit.
    fn remember(&mut self, dir: PathBuf, tracks: Vec<Track>) {
        const KEEP: usize = 64;
        if self.memo.insert(dir.clone(), tracks).is_none() {
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
    pub fn listing_dir(&self) -> PathBuf {
        self.selected_folder()
            .map(|f| f.path.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// Descend into the highlighted folder.
    pub fn enter_folder(&mut self) {
        let Some(folder) = self.selected_folder() else {
            return;
        };
        let target = folder.path.clone();
        let subdirs = library::list_subdirs(&target);
        // A leaf album has nothing to descend into; the right pane already shows it.
        if subdirs.is_empty() {
            self.focus = Pane::Tracks;
            return;
        }
        self.trail
            .push((self.cwd.clone(), self.folder_state.selected().unwrap_or(0)));
        self.cwd = target;
        self.folders = subdirs;
        self.folder_state = TableState::default();
        self.folder_state.select(Some(0));
        self.reload_tracks();
    }

    /// Step back up, restoring the row we came from. Stops at the root.
    pub fn leave_folder(&mut self) {
        let Some((parent, selected)) = self.trail.pop() else {
            return;
        };
        self.cwd = parent;
        self.folders = library::list_subdirs(&self.cwd);
        self.folder_state = TableState::default();
        if !self.folders.is_empty() {
            self.folder_state
                .select(Some(selected.min(self.folders.len() - 1)));
        }
        self.reload_tracks();
    }

    pub fn can_leave(&self) -> bool {
        !self.trail.is_empty()
    }

    /// Breadcrumb for the pane title: the root's name plus the way down.
    pub fn here(&self) -> String {
        let root_name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string());
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
        self.play_queue_index(idx);
    }

    fn play_queue_index(&mut self, idx: usize) {
        let Some(track) = self.queue.get(idx) else {
            return;
        };
        let path = track.path.clone();
        match self.audio.play_file(&path) {
            Ok(()) => {
                self.queue_pos = Some(idx);
                self.status = format!("playing {}", path.display());
                self.request_cover(&path);
            }
            Err(err) => {
                self.status = format!("error: {err:#}");
            }
        }
    }

    /// Hand the cover off to the worker and carry on. The old cover is cleared at
    /// once so a stale image never sits under a new track's title.
    fn request_cover(&mut self, path: &Path) {
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
        let Some(path) = self.now_playing().map(|t| t.path.clone()) else {
            return;
        };
        self.request_cover(&path);
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

    pub fn next_track(&mut self) {
        match self.queue_pos {
            Some(current) if current + 1 < self.queue.len() => {
                self.play_queue_index(current + 1);
            }
            Some(_) => {
                self.audio.stop();
                self.queue_pos = None;
                self.status = "end of queue".into();
            }
            // Nothing queued yet: start from whatever is on screen.
            None => self.play_selected_track(),
        }
    }

    pub fn prev_track(&mut self) {
        if let Some(i) = self.queue_pos.filter(|i| *i > 0) {
            self.play_queue_index(i - 1);
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
                Command::Stop => {
                    self.audio.stop();
                    self.queue_pos = None;
                }
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
    pub fn tick(&mut self) {
        if self.queue_pos.is_some() && self.audio.is_finished() && !self.audio.is_paused() {
            self.next_track();
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let (len, state) = match self.focus {
            Pane::Folders => (self.folders.len(), &mut self.folder_state),
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
            Pane::Folders => (self.folder_rows, &self.folder_state, self.folders.len()),
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
            Pane::Folders => self.select_folder(index),
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
        match self.pane_at(pos) {
            Some(Pane::Folders) => {
                let next = self.folder_state.selected().unwrap_or(0) as isize + delta;
                self.select_folder(
                    next.clamp(0, self.folders.len().saturating_sub(1) as isize) as usize
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
        match self.audio.seek(target) {
            Ok(()) => self.status = format!("seek {}", library::fmt_duration(target)),
            Err(err) => self.status = format!("seek failed: {err:#}"),
        }
    }

    /// Nudge the playhead by `delta` seconds, clamped to the track.
    pub fn seek_by(&mut self, delta: i64) {
        let Some(total) = self.now_playing().and_then(|t| t.duration) else {
            return;
        };
        let now = self.audio.position().as_secs_f64();
        let target = (now + delta as f64).clamp(0.0, total.as_secs_f64());
        let target = Duration::from_secs_f64(target);
        match self.audio.seek(target) {
            Ok(()) => self.status = format!("seek {}", library::fmt_duration(target)),
            Err(err) => self.status = format!("seek failed: {err:#}"),
        }
    }

    /// Selecting a folder rescans it, so skip the work when nothing changed.
    fn select_folder(&mut self, index: usize) {
        if index >= self.folders.len() || self.folder_state.selected() == Some(index) {
            return;
        }
        self.folder_state.select(Some(index));
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

        /// Ready to assert on: the first listing has already landed.
        fn app(&self) -> App {
            let mut app = App::new(
                self.0.clone(),
                Picker::halfblocks(),
                media::Bridge::detached(),
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
        app.folder_state.select(Some(1)); // Artist/, which holds no files itself
        app.reload_tracks();
        app.wait_for_tracks();
        assert_eq!(app.tracks.len(), 6, "both albums of the artist");
    }

    #[test]
    fn enter_descends_and_backspace_returns_to_the_same_row() {
        let lib = Library::new("descend");
        let mut app = lib.app();
        app.folder_state.select(Some(1)); // Artist/
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
        assert_eq!(app.folder_state.selected(), Some(1), "row restored");
        assert!(!app.can_leave(), "the root is the floor");
    }

    /// A leaf album has nothing below it, so Enter moves to the tracks instead of
    /// leaving the pane empty.
    #[test]
    fn entering_a_leaf_moves_focus_instead() {
        let lib = Library::new("leaf");
        let mut app = lib.app();
        app.folder_state.select(Some(0)); // Alpha/, files only
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
        app.folder_state.select(Some(1));
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

    /// A second visit to a folder must come from the memo, not another scan.
    #[test]
    fn revisiting_a_folder_is_served_from_memory() {
        let lib = Library::new("memo");
        let mut app = lib.app();
        app.folder_state.select(Some(1));
        app.reload_tracks();
        app.wait_for_tracks();

        app.folder_state.select(Some(0));
        app.reload_tracks();
        assert!(!app.tracks_loading, "Alpha should have been remembered");
        assert_eq!(app.tracks.len(), 3);
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

        app.click(Position { x: 2, y: 2 }, Instant::now());
        app.wait_for_tracks();

        assert_eq!(app.focus, Pane::Folders);
        assert_eq!(app.folder_state.selected(), Some(1), "second row");
        // Row 1 is Artist/, whose six tracks live in two subfolders.
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
            Some(0),
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
        app.seek_by(-600); // far before the start
        assert!(!app.status.contains("failed"), "{}", app.status);
        let at_start = app.audio.position();
        assert!(
            at_start < Duration::from_millis(500),
            "should have clamped to the start, got {at_start:?}"
        );

        app.seek_by(600); // far past the 2 s end
        assert!(!app.status.contains("failed"), "{}", app.status);
        let at_end = app.audio.position();
        assert!(
            at_end >= Duration::from_millis(1500),
            "should have clamped to the end, got {at_end:?}"
        );
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
