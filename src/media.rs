//! OS media-key integration.
//!
//! Media keys never reach a terminal application through stdin — the OS grabs them
//! first. So there is no cheaper "works while the window is focused" version: either
//! we register with the OS and it works everywhere including in the background, or
//! it does not work at all.
//!
//! | Platform | Mechanism | Notes |
//! |----------|-----------|-------|
//! | macOS | `MPRemoteCommandCenter` | needs a run loop on the **main** thread |
//! | Linux | MPRIS over D-Bus | also gives `playerctl` and desktop widgets |
//! | Windows | `SystemMediaTransportControls` | needs an HWND; a console has one |
//!
//! The macOS requirement is why the TUI runs on a worker thread and this half stays
//! on the main one: AppKit only delivers to the main thread's run loop.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender, TryIter};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

/// How long the host blocks per pump. Short enough that quitting feels immediate.
const PUMP: Duration = Duration::from_millis(100);

/// A media command, already translated out of souvlaki's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Relative seek in seconds; negative is backwards.
    SeekBy(i64),
    SetPosition(Duration),
}

impl Command {
    /// `None` for events we have nothing sensible to do with.
    fn from_event(event: MediaControlEvent) -> Option<Self> {
        // Default nudge for the coarse Seek variant, which carries no amount.
        const STEP: i64 = 5;
        Some(match event {
            MediaControlEvent::Toggle => Command::Toggle,
            MediaControlEvent::Play => Command::Play,
            MediaControlEvent::Pause => Command::Pause,
            MediaControlEvent::Stop => Command::Stop,
            MediaControlEvent::Next => Command::Next,
            MediaControlEvent::Previous => Command::Previous,
            MediaControlEvent::Seek(SeekDirection::Forward) => Command::SeekBy(STEP),
            MediaControlEvent::Seek(SeekDirection::Backward) => Command::SeekBy(-STEP),
            MediaControlEvent::SeekBy(direction, amount) => {
                let secs = amount.as_secs() as i64;
                match direction {
                    SeekDirection::Forward => Command::SeekBy(secs),
                    SeekDirection::Backward => Command::SeekBy(-secs),
                }
            }
            MediaControlEvent::SetPosition(MediaPosition(at)) => Command::SetPosition(at),
            // Volume, OpenUri and anything added later.
            _ => return None,
        })
    }
}

/// What the app tells the OS about the current track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<Duration>,
    pub elapsed: Duration,
    pub playing: bool,
    /// Path to the cached cover, shown in Control Center and desktop widgets.
    pub cover: Option<PathBuf>,
}

/// The half the app holds: commands in, metadata out.
pub struct Bridge {
    commands: Receiver<Command>,
    updates: Sender<NowPlaying>,
}

impl Bridge {
    /// Non-blocking. Safe to call every frame.
    pub fn commands(&self) -> TryIter<'_, Command> {
        self.commands.try_iter()
    }

    /// Publish the current track. Failure means the host is gone, which is fine.
    pub fn publish(&self, now: NowPlaying) {
        let _ = self.updates.send(now);
    }

    /// A bridge with nothing on the other end: no commands ever arrive and updates
    /// go nowhere. What `--no-media` installs, and what the tests use.
    pub fn detached() -> Self {
        let (_, commands) = mpsc::channel();
        let (updates, _) = mpsc::channel();
        Self { commands, updates }
    }
}

/// The half that must stay on the main thread.
pub struct Host {
    controls: MediaControls,
    updates: Receiver<NowPlaying>,
    /// Skip redundant OS calls; publishing runs on a timer.
    last: Option<NowPlaying>,
}

impl Host {
    /// Service the OS for up to [`PUMP`], applying any queued metadata.
    pub fn pump(&mut self) {
        // Keep only the newest; intermediate states are of no interest to the OS.
        let mut newest = None;
        while let Ok(update) = self.updates.try_recv() {
            newest = Some(update);
        }
        if let Some(now) = newest
            && self.last.as_ref() != Some(&now)
        {
            self.apply(&now);
            self.last = Some(now);
        }
        run_loop_for(PUMP);
    }

    fn apply(&mut self, now: &NowPlaying) {
        let cover_url = now
            .cover
            .as_ref()
            .and_then(|path| path.to_str())
            .map(|path| format!("file://{path}"));

        let _ = self.controls.set_metadata(MediaMetadata {
            title: Some(&now.title),
            artist: Some(&now.artist),
            album: Some(&now.album),
            cover_url: cover_url.as_deref(),
            duration: now.duration,
        });

        let progress = Some(MediaPosition(now.elapsed));
        let _ = self.controls.set_playback(if now.playing {
            MediaPlayback::Playing { progress }
        } else {
            MediaPlayback::Paused { progress }
        });
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.controls.detach();
    }
}

/// Register with the OS.
///
/// The [`Bridge`] is always returned so the app needs no conditional paths. The
/// [`Host`] is `None` when the OS declined — a missing feature, not an error, so
/// the reason is handed back for the status line rather than being fatal.
pub fn start() -> (Bridge, Option<Host>, Option<String>) {
    let (command_tx, commands) = mpsc::channel();
    let (updates_tx, updates) = mpsc::channel();
    let bridge = Bridge {
        commands,
        updates: updates_tx,
    };

    let config = PlatformConfig {
        display_name: "tuneterm",
        dbus_name: "tuneterm",
        hwnd: console_hwnd(),
    };

    let mut controls = match MediaControls::new(config) {
        Ok(controls) => controls,
        Err(err) => return (bridge, None, Some(format!("media keys off: {err:?}"))),
    };

    // The OS may call this from a thread of its own choosing, and `Sender` is not
    // `Sync`, so it goes behind a mutex.
    let sink = Mutex::new(command_tx);
    let attached = controls.attach(move |event| {
        if let Some(command) = Command::from_event(event)
            && let Ok(sink) = sink.lock()
        {
            let _ = sink.send(command);
        }
    });
    if let Err(err) = attached {
        return (bridge, None, Some(format!("media keys off: {err:?}")));
    }

    let host = Host {
        controls,
        updates,
        last: None,
    };
    (bridge, Some(host), None)
}

/// An HWND for souvlaki. A console application has a window even under a pseudo
/// console, though it may be hidden — whether Windows accepts a ConPTY host window
/// for a media session is unverified.
#[cfg(windows)]
fn console_hwnd() -> Option<*mut std::ffi::c_void> {
    let hwnd = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as *mut std::ffi::c_void)
    }
}

#[cfg(not(windows))]
fn console_hwnd() -> Option<*mut std::ffi::c_void> {
    None
}

/// Give the OS a slice of the main thread.
#[cfg(target_os = "macos")]
fn run_loop_for(duration: Duration) {
    use core_foundation_sys::runloop::{CFRunLoopRunInMode, kCFRunLoopDefaultMode};
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, duration.as_secs_f64(), 0);
    }
}

/// Elsewhere the backend runs its own thread, so the main thread only has to wait.
#[cfg(not(target_os = "macos"))]
fn run_loop_for(duration: Duration) {
    std::thread::sleep(duration);
}

/// Cut the current [`run_loop_for`] short so quitting does not wait for it.
#[cfg(target_os = "macos")]
pub fn wake() {
    use core_foundation_sys::runloop::{CFRunLoopGetMain, CFRunLoopStop};
    unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
}

#[cfg(not(target_os = "macos"))]
pub fn wake() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_the_events_we_act_on() {
        for (event, want) in [
            (MediaControlEvent::Toggle, Command::Toggle),
            (MediaControlEvent::Play, Command::Play),
            (MediaControlEvent::Pause, Command::Pause),
            (MediaControlEvent::Stop, Command::Stop),
            (MediaControlEvent::Next, Command::Next),
            (MediaControlEvent::Previous, Command::Previous),
            (
                MediaControlEvent::Seek(SeekDirection::Forward),
                Command::SeekBy(5),
            ),
            (
                MediaControlEvent::Seek(SeekDirection::Backward),
                Command::SeekBy(-5),
            ),
            (
                MediaControlEvent::SeekBy(SeekDirection::Forward, Duration::from_secs(30)),
                Command::SeekBy(30),
            ),
            (
                MediaControlEvent::SeekBy(SeekDirection::Backward, Duration::from_secs(12)),
                Command::SeekBy(-12),
            ),
            (
                MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(61))),
                Command::SetPosition(Duration::from_secs(61)),
            ),
        ] {
            assert_eq!(Command::from_event(event.clone()), Some(want), "{event:?}");
        }
    }

    /// Unknown or irrelevant events must be dropped, not mapped to something wrong.
    #[test]
    fn ignores_events_it_has_no_action_for() {
        assert_eq!(Command::from_event(MediaControlEvent::SetVolume(0.5)), None);
        assert_eq!(
            Command::from_event(MediaControlEvent::OpenUri("http://x".into())),
            None
        );
    }

    /// The bridge must survive a missing host, since media keys are optional.
    #[test]
    fn bridge_tolerates_a_dropped_host() {
        let (_tx, commands) = mpsc::channel();
        let (updates, rx) = mpsc::channel();
        let bridge = Bridge { commands, updates };
        drop(rx);
        bridge.publish(NowPlaying::default()); // must not panic
        assert_eq!(bridge.commands().count(), 0);
    }

    #[test]
    fn commands_arrive_in_order_and_drain_once() {
        let (tx, commands) = mpsc::channel();
        let (updates, _rx) = mpsc::channel();
        let bridge = Bridge { commands, updates };

        tx.send(Command::Next).unwrap();
        tx.send(Command::Toggle).unwrap();
        let drained: Vec<_> = bridge.commands().collect();
        assert_eq!(drained, vec![Command::Next, Command::Toggle]);
        assert_eq!(bridge.commands().count(), 0, "drained twice");
    }
}
