use std::fs::File;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink};

/// Thin wrapper over rodio. Keeps the device sink alive for the process lifetime.
pub struct AudioPlayer {
    // Dropping this stops all audio, so it must be held.
    _device: MixerDeviceSink,
    player: rodio::Player,
}

impl AudioPlayer {
    pub fn new() -> Result<Self> {
        let mut device =
            DeviceSinkBuilder::open_default_sink().context("no default audio output device")?;
        // Otherwise rodio prints a notice to stderr on drop, which lands on top
        // of the restored terminal.
        device.log_on_drop(false);
        let player = rodio::Player::connect_new(device.mixer());
        Ok(Self {
            _device: device,
            player,
        })
    }

    /// Replace whatever is queued with a remote file and start playing.
    ///
    /// The length is handed to the decoder explicitly. `Decoder::try_from` reads it
    /// from a `File`'s metadata, which a network stream has none of, and without it
    /// symphonia refuses to seek backwards — the same trap a `BufReader` sets for
    /// local files. `BufReader` is still wanted here, to turn the decoder's many
    /// small reads into few requests.
    pub fn play_url(&self, url: &str) -> Result<()> {
        let remote = crate::net::HttpFile::open(url).map_err(|e| anyhow::anyhow!(e))?;
        let len = remote.len();
        let source = rodio::Decoder::builder()
            .with_data(std::io::BufReader::with_capacity(256 * 1024, remote))
            .with_byte_len(len)
            .with_seekable(true)
            .build()
            .with_context(|| format!("decode {url}"))?;
        self.player.clear();
        self.player.append(source);
        self.player.play();
        Ok(())
    }

    /// Replace whatever is queued with `path` and start playing.
    pub fn play_file(&self, path: &Path) -> Result<()> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // Hand the `File` over directly. Wrapping it in a `BufReader` first hides
        // the stream length, and without that symphonia refuses to seek backwards.
        let source =
            Decoder::try_from(file).with_context(|| format!("decode {}", path.display()))?;
        self.player.clear();
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

    pub fn stop(&self) {
        self.player.clear();
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

    /// Jump to `pos`. Not every decoder supports it, hence the error. Note that
    /// rodio reports success without seeking when nothing is queued.
    pub fn seek(&self, pos: Duration) -> Result<()> {
        self.player.try_seek(pos).context("seek")
    }

    pub fn volume(&self) -> rodio::Float {
        self.player.volume()
    }

    pub fn nudge_volume(&self, delta: rodio::Float) {
        let v = (self.player.volume() + delta).clamp(0.0, 2.0);
        self.player.set_volume(v);
    }
}

#[cfg(test)]
mod real_file_check {
    use super::*;

    /// Manual check against a real mp3: `cargo test mp3_seek -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mp3_seek_actually_moves() {
        let path = std::env::var("TUNETERM_FILE").expect("set TUNETERM_FILE");
        let player = AudioPlayer::new().unwrap();
        player.play_file(Path::new(&path)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let before = player.position();
        player.seek(Duration::from_secs(90)).expect("seek");
        let after = player.position();
        println!("before={before:?} after={after:?}");
        assert!(after > Duration::from_secs(80), "forward seek: {after:?}");

        player.seek(Duration::from_secs(5)).expect("seek back");
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
        let player = AudioPlayer::new().unwrap();

        let start = std::time::Instant::now();
        player.play_url(url).expect("play");
        println!("time to first audio: {:?}", start.elapsed());

        std::thread::sleep(Duration::from_millis(400));
        assert!(!player.is_finished(), "stream ended immediately");
        let early = player.position();
        println!("position after 400ms: {early:?}");

        // Seeking is the part that needs the byte length and range requests.
        let t = std::time::Instant::now();
        player.seek(Duration::from_secs(600)).expect("seek");
        println!("seek to 10:00 took {:?}", t.elapsed());
        let after = player.position();
        println!("position after seek: {after:?}");
        assert!(
            after >= Duration::from_secs(590),
            "seek did not take: {after:?}"
        );

        player.seek(Duration::from_secs(30)).expect("seek back");
        let back = player.position();
        println!("position after seeking back: {back:?}");
        assert!(
            back < Duration::from_secs(120),
            "backward seek failed: {back:?}"
        );
    }
}
