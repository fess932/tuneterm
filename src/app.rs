use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::cover::{self, CoverLoader};
use crate::library::{self, Folder, Track};
use crate::player::AudioPlayer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Folders,
    Tracks,
}

pub struct App {
    pub root: PathBuf,
    pub folders: Vec<Folder>,
    pub tracks: Vec<Track>,
    pub folder_state: TableState,
    pub track_state: TableState,
    pub focus: Pane,

    /// Index into `tracks` of the track currently loaded into the player.
    pub playing: Option<usize>,
    pub cover: Option<StatefulProtocol>,
    /// Pixel size of the cover as it will be drawn. Needed in full, not just as a
    /// ratio: a cover is never enlarged, so the drawn size — and therefore the
    /// centring — depends on the source size.
    pub cover_size: Option<(u32, u32)>,
    /// True between asking the worker for a cover and getting an answer.
    pub cover_pending: bool,
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

    pub status: String,
    pub should_quit: bool,
}

/// Two clicks on the same row within this window count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

impl App {
    pub fn new(root: PathBuf, picker: Picker) -> Result<Self> {
        let folders = library::scan_folders(&root, 5);
        let mut folder_state = TableState::default();
        if !folders.is_empty() {
            folder_state.select(Some(0));
        }

        let mut app = Self {
            root,
            folders,
            tracks: Vec::new(),
            folder_state,
            track_state: TableState::default(),
            focus: Pane::Folders,
            playing: None,
            cover: None,
            picker,
            audio: AudioPlayer::new()?,
            cover_size: None,
            cover_pending: false,
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
            status: String::new(),
            should_quit: false,
        };
        app.status = if app.folders.is_empty() {
            format!("no audio found under {}", app.root.display())
        } else {
            format!("{} folders", app.folders.len())
        };
        app.open_selected_folder();
        Ok(app)
    }

    pub fn selected_folder(&self) -> Option<&Folder> {
        self.folders.get(self.folder_state.selected()?)
    }

    pub fn now_playing(&self) -> Option<&Track> {
        self.tracks.get(self.playing?)
    }

    pub fn open_selected_folder(&mut self) {
        let Some(folder) = self.selected_folder() else {
            return;
        };
        self.tracks = library::scan_tracks(&folder.path.clone());
        self.track_state = TableState::default();
        if !self.tracks.is_empty() {
            self.track_state.select(Some(0));
        }
        // Track indices refer to the old folder; they are meaningless now.
        self.playing = None;
    }

    pub fn play_selected_track(&mut self) {
        let Some(idx) = self.track_state.selected() else {
            return;
        };
        self.play_index(idx);
    }

    pub fn play_index(&mut self, idx: usize) {
        let Some(track) = self.tracks.get(idx) else {
            return;
        };
        let path = track.path.clone();
        match self.audio.play_file(&path) {
            Ok(()) => {
                self.playing = Some(idx);
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
        self.cover = None;
        self.cover_size = None;
        self.cover_pending = true;
        self.cover_generation += 1;
        self.cover_requested_box = self.art_box_px();
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
        match loaded.image {
            Some(img) => {
                self.cover_size = Some((img.width().max(1), img.height().max(1)));
                self.cover = Some(self.picker.new_resize_protocol(img));
            }
            None => {
                self.cover_size = None;
                self.cover = None;
            }
        }
    }

    pub fn toggle_play(&mut self) {
        if self.playing.is_none() {
            self.play_selected_track();
        } else {
            self.audio.toggle();
        }
    }

    pub fn next_track(&mut self) {
        let next = self.playing.map(|i| i + 1).unwrap_or(0);
        if next < self.tracks.len() {
            self.play_index(next);
        } else {
            self.audio.stop();
            self.playing = None;
            self.status = "end of folder".into();
        }
    }

    pub fn prev_track(&mut self) {
        if let Some(i) = self.playing.filter(|i| *i > 0) {
            self.play_index(i - 1);
        }
    }

    /// Advance automatically when the current source has drained.
    pub fn tick(&mut self) {
        if self.playing.is_some() && self.audio.is_finished() && !self.audio.is_paused() {
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
            self.open_selected_folder();
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
        self.open_selected_folder();
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
    struct Library(PathBuf);

    impl Library {
        fn new(name: &str) -> Self {
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
            Self(root)
        }

        fn app(&self) -> App {
            App::new(self.0.clone(), Picker::halfblocks()).expect("app init")
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

    #[test]
    fn scans_the_fixture() {
        let lib = Library::new("scan");
        let app = lib.app();
        assert_eq!(app.folders.len(), 2, "two albums");
        assert_eq!(app.tracks.len(), 3, "first album opened");
    }

    #[test]
    fn clicking_a_track_row_selects_it_without_playing() {
        let lib = Library::new("click-track");
        let mut app = lib.app();
        app.track_rows = rows(30, 1, 10);

        app.click(Position { x: 32, y: 3 }, Instant::now());

        assert_eq!(app.focus, Pane::Tracks, "focus follows the click");
        assert_eq!(app.track_state.selected(), Some(2), "third row");
        assert!(app.playing.is_none(), "one click must not start audio");
    }

    #[test]
    fn clicking_a_folder_row_switches_album() {
        let lib = Library::new("click-folder");
        let mut app = lib.app();
        app.folder_rows = rows(0, 1, 10);

        app.click(Position { x: 2, y: 2 }, Instant::now());

        assert_eq!(app.focus, Pane::Folders);
        assert_eq!(app.folder_state.selected(), Some(1), "second album");
        assert_eq!(app.tracks.len(), 2, "its tracks got loaded");
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
        assert!(app.playing.is_some(), "playback failed: {}", app.status);

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
        assert!(app.playing.is_some(), "playback failed: {}", app.status);

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
        assert!(app.playing.is_none());
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
        assert_eq!(app.playing, Some(0), "next started playback");

        app.click(Position { x: 20, y: 21 }, Instant::now());
        assert_eq!(app.playing, Some(1), "next advanced");

        app.click(Position { x: 3, y: 21 }, Instant::now());
        assert_eq!(app.playing, Some(0), "prev went back");

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

        assert!(app.playing.is_some(), "playback started: {}", app.status);
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
        let landed_on = app.playing;

        // Let the worker settle, then take whatever it produced.
        for _ in 0..200 {
            app.poll_cover();
            if !app.cover_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(app.playing, landed_on, "playback did not move on its own");
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
        assert!(app.playing.is_none());
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

        let mut app = App::new(root.clone(), Picker::halfblocks()).unwrap();
        app.art_budget = Rect::new(0, 0, 30, 15);

        for round in 0..4 {
            let t = Instant::now();
            app.play_index(round);
            let blocked = t.elapsed();

            let t = Instant::now();
            let mut waited = Duration::ZERO;
            while app.cover_pending && waited < Duration::from_secs(5) {
                app.poll_cover();
                std::thread::sleep(Duration::from_millis(5));
                waited = t.elapsed();
            }
            println!(
                "  play_index blocked {:>8.1?}   cover ready after {:>8.1?}   size {:?}",
                blocked, waited, app.cover_size
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
