use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink};

use crate::worker::{Cancel, Wake, Worker};

/// A remote track, decoded far enough to play.
pub type RemoteSource = Decoder<std::io::BufReader<crate::net::HttpFile>>;

/// Open `url` and get it ready for [`AudioPlayer::play_remote`].
///
/// **Blocking, and not for the thread that draws.** Opening a stream is a length
/// probe, a range request and a header read: a second or two on a slow link, and
/// up to the 20s timeout in `net` against a host that never answers.
///
/// The length is handed to the decoder explicitly. `Decoder::try_from` reads it
/// from a `File`'s metadata, which a network stream has none of, and without it
/// symphonia refuses to seek backwards — the same trap a `BufReader` sets for
/// local files. `BufReader` is still wanted here, to turn the decoder's many
/// small reads into few requests.
pub fn open_url(url: &str) -> Result<RemoteSource> {
    let remote = crate::net::HttpFile::open(url).map_err(|e| anyhow::anyhow!(e))?;
    let len = remote.len();
    rodio::Decoder::builder()
        .with_data(std::io::BufReader::with_capacity(256 * 1024, remote))
        .with_byte_len(len)
        .with_seekable(true)
        .build()
        .with_context(|| format!("decode {url}"))
}

/// Thin wrapper over rodio. Keeps the device sink alive for the process lifetime.
///
/// # Nothing here waits on the audio thread
///
/// Two of rodio's controls block until the device's callback acknowledges them:
/// `Player::clear` waits for the queue to drain, and `Player::try_seek` waits for
/// the answer to its seek order. Both are answered from the callback, and the
/// callback stops running for good when the output device goes away — unplug
/// headphones mid-track and CoreAudio never calls back again, because cpal pins
/// its audio unit to the device it opened (`kAudioOutputUnitProperty_CurrentDevice`)
/// rather than following the default.
///
/// Called from the thread that draws, either one is a permanent freeze: no
/// redraw, no keys, not even `q`. So neither is called from there. Clearing asks
/// without waiting, and seeking happens on a thread of its own.
pub struct AudioPlayer {
    // Dropping this stops all audio, so it must be held.
    _device: MixerDeviceSink,
    player: Arc<rodio::Player>,
    /// Seeks, off the caller's thread. One at a time and replaceable: dragging the
    /// progress bar produces them faster than the 5 ms it takes to answer one, and
    /// only the last is worth anything.
    seek: Worker<Duration, Result<(), String>>,
    seek_generation: AtomicU64,
}

impl AudioPlayer {
    /// `wake` is what the seek thread rings when it has an answer, so a failure
    /// reaches the status line without the loop coming back to ask.
    pub fn new(wake: Wake) -> Result<Self> {
        let mut device =
            DeviceSinkBuilder::open_default_sink().context("no default audio output device")?;
        // Otherwise rodio prints a notice to stderr on drop, which lands on top
        // of the restored terminal.
        device.log_on_drop(false);
        let player = Arc::new(rodio::Player::connect_new(device.mixer()));

        let seeker = Arc::clone(&player);
        let seek = Worker::spawn("seek", wake, move |pos: Duration, _: &Cancel| {
            seeker.try_seek(pos).map_err(|err| format!("{err}"))
        });

        Ok(Self {
            _device: device,
            player,
            seek,
            seek_generation: AtomicU64::new(0),
        })
    }

    /// Replace whatever is queued with a stream [`open_url`] has already opened.
    ///
    /// Everything slow happened there; this is just handing it to rodio.
    pub fn play_remote(&self, source: RemoteSource) {
        clear_without_waiting(&self.player);
        self.player.append(source);
        self.player.play();
    }

    /// Replace whatever is queued with `path` and start playing.
    pub fn play_file(&self, path: &Path) -> Result<()> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // Hand the `File` over directly. Wrapping it in a `BufReader` first hides
        // the stream length, and without that symphonia refuses to seek backwards.
        let source =
            Decoder::try_from(file).with_context(|| format!("decode {}", path.display()))?;
        clear_without_waiting(&self.player);
        self.player.append(source);
        self.player.play();
        Ok(())
    }

    pub fn toggle(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    /// Hold, without touching the queue. Used to keep a restored track quiet
    /// until its playhead has been put where it belongs.
    pub fn pause(&self) {
        self.player.pause();
    }

    /// Release a hold. No effect if nothing is paused.
    pub fn play(&self) {
        self.player.play();
    }

    pub fn stop(&self) {
        clear_without_waiting(&self.player);
    }

    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }

    /// True once the queued source has played out.
    pub fn is_finished(&self) -> bool {
        self.player.empty()
    }

    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    /// Ask for a jump to `pos`, and return.
    ///
    /// The answer arrives later through [`Self::seek_error`] — the seek itself has
    /// to be waited for, and this is not the thread to wait on. Not every decoder
    /// supports seeking, hence the error at all; note also that rodio reports
    /// success without seeking when nothing is queued.
    pub fn seek(&self, pos: Duration) {
        let generation = self.seek_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.seek.request(generation, pos);
    }

    /// The answer to the last seek, once, or `None` while none has arrived.
    ///
    /// Both outcomes are reported, not just the failures: a caller waiting to
    /// start playback at the restored position needs to know the playhead has
    /// moved, and a seek that could not be performed should not leave it waiting
    /// forever.
    pub fn seek_result(&self) -> Option<Result<(), String>> {
        self.seek.drain().map(|(_, result)| result).last()
    }

    /// Block until the outstanding seek has been answered. For tests, which assert
    /// on the playhead right after moving it; nothing in the app may wait like this.
    #[cfg(test)]
    pub fn wait_for_seek(&self) -> Option<String> {
        let wanted = self.seek_generation.load(Ordering::Relaxed);
        if wanted == 0 {
            return None; // nothing was ever asked for, so nothing is owed
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            for (generation, result) in self.seek.drain() {
                if generation >= wanted {
                    return result.err();
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Some("the seek was never answered".into())
    }

    pub fn volume(&self) -> rodio::Float {
        self.player.volume()
    }

    pub fn set_volume(&self, volume: rodio::Float) {
        self.player.set_volume(volume.clamp(0.0, MAX_VOLUME));
    }

    pub fn nudge_volume(&self, delta: rodio::Float) {
        self.set_volume(self.player.volume() + delta);
    }
}

/// Drop what is queued without waiting for the audio thread to confirm it.
///
/// `Player::clear` is this plus a blocking wait; see [`AudioPlayer`] for why that
/// wait cannot be allowed to happen on the interface thread. Asking alone costs at
/// most the 5 ms of the old source already in flight, which is well under the
/// point where a track change sounds like anything at all.
///
/// A free function so the test below can aim it at a player whose audio thread
/// never runs, which is what a disconnected output device amounts to.
fn clear_without_waiting(player: &rodio::Player) {
    for _ in 0..player.len() {
        player.skip_one();
    }
    player.pause();
}

/// The ceiling on gain. Shared with the settings file so a hand-edited one cannot
/// ask for more than the `+` key can.
const MAX_VOLUME: rodio::Float = crate::config::MAX_VOLUME;

#[cfg(test)]
mod real_file_check {
    use super::*;

    /// Manual check against a real mp3: `cargo test mp3_seek -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mp3_seek_actually_moves() {
        let path = std::env::var("TUNETERM_FILE").expect("set TUNETERM_FILE");
        let player = AudioPlayer::new(Wake::none()).unwrap();
        player.play_file(Path::new(&path)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let before = player.position();
        player.seek(Duration::from_secs(90));
        assert_eq!(player.wait_for_seek(), None, "seek");
        let after = player.position();
        println!("before={before:?} after={after:?}");
        assert!(after > Duration::from_secs(80), "forward seek: {after:?}");

        player.seek(Duration::from_secs(5));
        assert_eq!(player.wait_for_seek(), None, "seek back");
        let back = player.position();
        println!("back={back:?}");
        assert!(back < Duration::from_secs(20), "backward seek: {back:?}");
    }
}

#[cfg(test)]
mod stream_check {
    use super::*;

    /// Play a real remote episode, seek in it, and confirm the playhead moved.
    /// `cargo test streams_a_real_episode -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn streams_a_real_episode() {
        let url = "https://datashat.net/music_for_programming_78-datassette.mp3";
        let player = AudioPlayer::new(Wake::none()).unwrap();

        let start = std::time::Instant::now();
        let source = open_url(url).expect("open");
        println!("time to open: {:?}", start.elapsed());
        player.play_remote(source);

        std::thread::sleep(Duration::from_millis(400));
        assert!(!player.is_finished(), "stream ended immediately");
        let early = player.position();
        println!("position after 400ms: {early:?}");

        // Seeking is the part that needs the byte length and range requests.
        let t = std::time::Instant::now();
        player.seek(Duration::from_secs(600));
        assert_eq!(player.wait_for_seek(), None, "seek");
        println!("seek to 10:00 took {:?}", t.elapsed());
        let after = player.position();
        println!("position after seek: {after:?}");
        assert!(
            after >= Duration::from_secs(590),
            "seek did not take: {after:?}"
        );

        player.seek(Duration::from_secs(30));
        assert_eq!(player.wait_for_seek(), None, "seek back");
        let back = player.position();
        println!("position after seeking back: {back:?}");
        assert!(
            back < Duration::from_secs(120),
            "backward seek failed: {back:?}"
        );
    }
}

#[cfg(test)]
mod device_loss {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    /// A player whose output is never read: no thread pulls its samples, which is
    /// exactly what an output device that has gone away leaves behind.
    fn player_with_no_audio_thread() -> rodio::Player {
        let (player, queue_end) = rodio::Player::new();
        // Deliberately leaked. Dropping the queue end would let the controls
        // notice and answer, and it is precisely *not* answering that is at issue.
        std::mem::forget(queue_end);
        player.append(rodio::buffer::SamplesBuffer::new(
            std::num::NonZero::new(1).unwrap(),
            std::num::NonZero::new(8000).unwrap(),
            vec![0.0f32; 8000],
        ));
        player
    }

    /// The freeze this exists to prevent: pull the headphones out mid-track and
    /// the next track change used to take the whole interface with it, because
    /// `Player::clear` waits for an acknowledgement from an audio thread that has
    /// stopped running. Nothing on the drawing thread may wait like that.
    #[test]
    fn clearing_returns_even_when_the_audio_thread_has_stopped() {
        let (done, answered) = mpsc::channel();
        thread::Builder::new()
            .name("clear-under-test".into())
            .spawn(move || {
                let player = player_with_no_audio_thread();
                clear_without_waiting(&player);
                let _ = done.send(());
                // Hold the player: dropping it would unblock a `clear` that is
                // still waiting, and this test must not depend on that.
                thread::sleep(Duration::from_secs(5));
            })
            .expect("spawn");

        assert!(
            answered.recv_timeout(Duration::from_secs(2)).is_ok(),
            "clearing blocked on an audio thread that will never answer"
        );
    }
}
