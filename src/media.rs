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

    let hwnd = media_window();
    // souvlaki panics on a missing HWND rather than returning an error, and on
    // Windows there is nothing to register without one.
    if cfg!(windows) && hwnd.is_none() {
        return (bridge, None, Some("media keys off: no window".into()));
    }

    let config = PlatformConfig {
        display_name: "tuneterm",
        dbus_name: "tuneterm",
        hwnd,
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

/// The window a Windows media session binds to. `None` everywhere else, where the
/// APIs are process-wide and know nothing about windows.
#[cfg(windows)]
fn media_window() -> Option<*mut std::ffi::c_void> {
    win::hidden_window()
}

#[cfg(not(windows))]
fn media_window() -> Option<*mut std::ffi::c_void> {
    None
}

/// A window of our own, and the message pump that goes with it.
///
/// `SystemMediaTransportControls` binds a session to a window and refuses one this
/// process does not own: handing it `GetConsoleWindow` fails with `E_ACCESSDENIED`,
/// because under a pseudo console that window belongs to conhost. A message-only
/// window is refused too, with `E_INVALIDARG`. So it has to be a real top-level
/// window — which is only an inbox for messages here. It is never shown, never
/// drawn, and `WS_EX_TOOLWINDOW` keeps it out of the taskbar and out of Alt-Tab.
///
/// A window belongs to the thread that created it, so both calls below have to
/// happen on the main thread — the one that services the OS.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE,
        MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT,
        RegisterClassW, WM_NULL, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    /// The thread currently inside [`pump`] — which is who [`wake`] has to reach.
    /// Addressing the window instead would post to whichever thread *created* it,
    /// and that is not necessarily the one waiting.
    static PUMPING: AtomicU32 = AtomicU32::new(0);
    /// Set by [`wake`], because the posted message alone only ends the *wait*.
    static WOKEN: AtomicBool = AtomicBool::new(false);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `None` when Windows refuses, which costs the media keys and nothing else.
    pub fn hidden_window() -> Option<*mut c_void> {
        let class = wide("tuneterm_media");
        let title = wide("tuneterm");
        let hwnd = unsafe {
            let instance = GetModuleHandleW(ptr::null());
            let mut spec: WNDCLASSW = std::mem::zeroed();
            spec.lpfnWndProc = Some(DefWindowProcW);
            spec.hInstance = instance as _;
            spec.lpszClassName = class.as_ptr();
            // Zero means it is registered already, which serves just as well.
            RegisterClassW(&spec);

            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                // No WS_VISIBLE, and ShowWindow is never called.
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance as _,
                ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        Some(hwnd)
    }

    /// Deliver whatever the OS has queued, for up to `slice`.
    pub fn pump(slice: Duration) {
        PUMPING.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        let deadline = Instant::now() + slice;
        loop {
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&msg);
                }
            }
            if WOKEN.swap(false, Ordering::AcqRel) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            // Sleep on the queue rather than spinning through the slice.
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    ptr::null(),
                    left.as_millis() as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
        }
    }

    /// End the current [`pump`] early. The flag is what it reads; the message is
    /// only there to end the wait it may be sitting in.
    pub fn wake() {
        WOKEN.store(true, Ordering::Release);
        let thread = PUMPING.load(Ordering::Acquire);
        if thread != 0 {
            unsafe { PostThreadMessageW(thread, WM_NULL, 0, 0) };
        }
    }
}

/// Give the OS a slice of the main thread.
#[cfg(target_os = "macos")]
fn run_loop_for(duration: Duration) {
    use core_foundation_sys::runloop::{CFRunLoopRunInMode, kCFRunLoopDefaultMode};
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, duration.as_secs_f64(), 0);
    }
}

/// Windows delivers the session's events through our window's message queue, so
/// the slice goes to pumping it.
#[cfg(windows)]
fn run_loop_for(duration: Duration) {
    win::pump(duration);
}

/// Elsewhere the backend runs its own thread, so the main thread only has to wait.
#[cfg(not(any(target_os = "macos", windows)))]
fn run_loop_for(duration: Duration) {
    std::thread::sleep(duration);
}

/// Cut the current [`run_loop_for`] short so quitting does not wait for it.
#[cfg(target_os = "macos")]
pub fn wake() {
    use core_foundation_sys::runloop::{CFRunLoopGetMain, CFRunLoopStop};
    unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
}

#[cfg(windows)]
pub fn wake() {
    win::wake();
}

#[cfg(not(any(target_os = "macos", windows)))]
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

    /// The whole reason `win` exists. Handing SMTC the console window fails with
    /// `E_ACCESSDENIED`, since under a pseudo console it belongs to conhost; ours
    /// has to be accepted, and has to stay off the screen.
    #[cfg(windows)]
    #[test]
    fn windows_accepts_our_window_and_never_shows_it() {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let hwnd = win::hidden_window().expect("no window");
        assert_eq!(
            unsafe { IsWindowVisible(hwnd) },
            0,
            "the media window must never be visible"
        );

        let controls = MediaControls::new(PlatformConfig {
            display_name: "tuneterm-test",
            dbus_name: "tuneterm-test",
            hwnd: Some(hwnd),
        });
        let mut controls = controls.expect("Windows refused the window");
        controls.attach(|_| {}).expect("attach");
        assert!(
            controls
                .set_playback(MediaPlayback::Paused { progress: None })
                .is_ok()
        );
    }

    /// End to end, against the real OS: `cargo test media_keys -- --ignored
    /// --nocapture`, then press play/pause within ten seconds. Ignored because it
    /// needs a keystroke, and because it registers a real media session.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn media_keys_reach_the_bridge() {
        let (bridge, host, warning) = start();
        let mut host = host.unwrap_or_else(|| panic!("no media host: {warning:?}"));
        // Windows routes the keys to a session that claims to be playing.
        bridge.publish(NowPlaying {
            title: "probe".into(),
            playing: true,
            ..Default::default()
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen: Vec<Command> = Vec::new();
        while std::time::Instant::now() < deadline && seen.is_empty() {
            host.pump();
            seen.extend(bridge.commands());
        }
        println!("commands: {seen:?}");
        assert!(!seen.is_empty(), "no media key arrived in ten seconds");
    }

    /// `wake` has to end a pump that is sitting on an empty queue, or quitting
    /// would wait out the slice.
    #[cfg(windows)]
    #[test]
    fn waking_ends_the_pump_early() {
        win::hidden_window().expect("no window");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            wake();
        });
        let started = std::time::Instant::now();
        win::pump(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "pump ran for {:?}, so wake did not reach it",
            started.elapsed()
        );
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
