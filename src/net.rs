//! HTTP, only as much of it as a player needs.
//!
//! The load-bearing part is [`HttpFile`]: rodio's decoder wants
//! `Read + Seek + Send + Sync`, and symphonia will not seek backwards unless it knows
//! the stream length. Both are satisfied by a `Content-Length` plus range requests,
//! which is exactly what a podcast enclosure offers.
//!
//! Nothing here is async. Fetching happens on a worker thread, so a blocking client
//! is both simpler and one fewer runtime to carry.

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

/// Long enough for a slow feed, short enough that a dead host does not look like a
/// hang. The worker is cancellable either way.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Cap on a fetched document. A feed is tens of kilobytes; anything vastly larger is
/// a wrong URL, not a feed, and should not be read into memory.
const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("tuneterm/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Fetch a whole document, for feeds and cover art.
pub fn get(url: &str) -> Result<Vec<u8>, String> {
    let mut response = agent().get(url).call().map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_DOCUMENT)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.is_empty() {
        return Err("empty response".into());
    }
    Ok(body)
}

/// A seekable window onto a remote file.
///
/// Reads run forwards through one open response; a seek drops it and the next read
/// opens a fresh one at the new offset. That is enough for a decoder, which reads
/// sequentially and seeks rarely.
pub struct HttpFile {
    url: String,
    len: u64,
    pos: u64,
    /// The response being read, if one is open at `pos`.
    stream: Option<Box<dyn Read + Send + Sync>>,
    agent: ureq::Agent,
}

impl HttpFile {
    /// Ask the server how big the file is and whether it will serve ranges.
    pub fn open(url: &str) -> Result<Self, String> {
        let agent = agent();
        let len = match probe_length(&agent, url) {
            Some(len) if len > 0 => len,
            _ => return Err("server would not report a size".into()),
        };
        Ok(Self {
            url: url.to_string(),
            len,
            pos: 0,
            stream: None,
            agent,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    fn open_stream(&mut self) -> io::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let response = self
            .agent
            .get(&self.url)
            .header("Range", format!("bytes={}-", self.pos))
            .call()
            .map_err(io::Error::other)?;
        self.stream = Some(Box::new(response.into_body().into_reader()));
        Ok(())
    }
}

impl Read for HttpFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        self.open_stream()?;
        let stream = self.stream.as_mut().expect("just opened");
        match stream.read(buf) {
            Ok(0) => {
                // The response ended. If there is more file, the next read reopens.
                self.stream = None;
                Ok(0)
            }
            Ok(n) => {
                self.pos += n as u64;
                Ok(n)
            }
            Err(err) => {
                self.stream = None;
                Err(err)
            }
        }
    }
}

impl Seek for HttpFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.len as i64 + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        let target = (target as u64).min(self.len);
        if target != self.pos {
            // Whatever is open is at the wrong offset now.
            self.stream = None;
            self.pos = target;
        }
        Ok(self.pos)
    }
}

/// Total size of the file, by whichever route the server allows.
///
/// `HEAD` is cheapest and usually enough. It is not always available, though —
/// musicforprogramming.net answers a `HEAD` with no `Content-Length` at all — so the
/// fallback asks for a single byte and reads the total out of `Content-Range`. That
/// has the useful side effect of proving the server serves ranges, which is the whole
/// premise of seeking here.
fn probe_length(agent: &ureq::Agent, url: &str) -> Option<u64> {
    if let Some(len) = head_length(agent, url) {
        return Some(len);
    }
    let response = agent.get(url).header("Range", "bytes=0-0").call().ok()?;
    // `Content-Range: bytes 0-0/102973`
    let range = response.headers().get("content-range")?.to_str().ok()?;
    range
        .rsplit_once('/')
        .and_then(|(_, total)| total.trim().parse().ok())
}

fn head_length(agent: &ureq::Agent, url: &str) -> Option<u64> {
    let head = agent.head(url).call().ok()?;
    head.headers()
        .get("content-length")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seek arithmetic, without a server: the offsets are the part that goes wrong.
    fn fake(len: u64) -> HttpFile {
        HttpFile {
            url: "https://example.invalid/x".into(),
            len,
            pos: 0,
            stream: None,
            agent: agent(),
        }
    }

    #[test]
    fn seeks_from_every_anchor() {
        let mut file = fake(1000);
        assert_eq!(file.seek(SeekFrom::Start(100)).unwrap(), 100);
        assert_eq!(file.seek(SeekFrom::Current(50)).unwrap(), 150);
        assert_eq!(file.seek(SeekFrom::Current(-100)).unwrap(), 50);
        assert_eq!(file.seek(SeekFrom::End(-1)).unwrap(), 999);
        assert_eq!(file.seek(SeekFrom::End(0)).unwrap(), 1000);
    }

    /// Past the end must clamp, not error: a decoder probing the tail is normal.
    #[test]
    fn seeking_past_the_end_clamps() {
        let mut file = fake(1000);
        assert_eq!(file.seek(SeekFrom::Start(9_999)).unwrap(), 1000);
        assert_eq!(file.seek(SeekFrom::End(500)).unwrap(), 1000);
    }

    #[test]
    fn seeking_before_the_start_is_an_error() {
        let mut file = fake(1000);
        assert!(file.seek(SeekFrom::Current(-1)).is_err());
        assert!(file.seek(SeekFrom::End(-2000)).is_err());
    }

    /// At the end there is nothing to read, and no request should be attempted —
    /// this runs against an unroutable host, so a request would fail the test.
    #[test]
    fn reading_at_the_end_returns_zero_without_a_request() {
        let mut file = fake(10);
        file.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(file.read(&mut buf).unwrap(), 0);
    }

    /// A seek must invalidate the open response, or the next read would return bytes
    /// from the old offset — silent corruption rather than an error.
    #[test]
    fn a_seek_drops_the_open_stream() {
        let mut file = fake(1000);
        file.stream = Some(Box::new(io::Cursor::new(vec![0u8; 16])));
        file.seek(SeekFrom::Start(500)).unwrap();
        assert!(file.stream.is_none());
    }

    /// Seeking to where we already are must not throw away a usable stream.
    #[test]
    fn seeking_to_the_current_position_keeps_the_stream() {
        let mut file = fake(1000);
        file.seek(SeekFrom::Start(200)).unwrap();
        file.stream = Some(Box::new(io::Cursor::new(vec![0u8; 16])));
        file.seek(SeekFrom::Start(200)).unwrap();
        assert!(file.stream.is_some(), "needless reconnect");
    }

    #[test]
    fn a_bad_url_fails_rather_than_hanging() {
        assert!(get("https://tuneterm.invalid/nothing").is_err());
        assert!(HttpFile::open("https://tuneterm.invalid/nothing").is_err());
    }

    /// Against the real thing: `cargo test net:: -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn reads_a_real_file_in_pieces() {
        for url in [
            "https://musicforprogramming.net/rss.xml",
            // The enclosure is what actually has to stream.
            "https://datashat.net/music_for_programming_78-datassette.mp3",
        ] {
            println!("\n--- {url}");
            check_seekable(url);
        }
    }

    fn check_seekable(url: &str) {
        let mut file = HttpFile::open(url).expect("open");
        println!("length: {}", file.len());
        let mut head = [0u8; 32];
        file.read_exact(&mut head).expect("read head");

        // Seek back and read the same bytes again; a broken reader returns something
        // else here.
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut again = [0u8; 32];
        file.read_exact(&mut again).expect("re-read");
        assert_eq!(head, again, "the same offset gave different bytes");

        file.seek(SeekFrom::Start(file.len() - 16)).unwrap();
        let mut tail = [0u8; 16];
        file.read_exact(&mut tail).expect("read tail");
        println!("tail read ok, {} bytes long", file.len());
    }
}
