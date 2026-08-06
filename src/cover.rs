use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryIter};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use image::DynamicImage;
use image::imageops::FilterType;

use crate::library;

pub struct Request {
    pub generation: u64,
    pub path: PathBuf,
    /// Longest side the cover could ever be drawn at, in pixels. Anything bigger
    /// is shrunk here so the render thread has nothing left to resize.
    pub max_px: u32,
}

pub struct Loaded {
    pub generation: u64,
    /// `None` means the track simply has no cover.
    pub image: Option<DynamicImage>,
}

/// Loads and pre-shrinks cover art on a worker thread.
///
/// Decoding a 1400 px JPEG costs ~190 ms and shrinking it ~400 ms in a debug
/// build, which would stall the UI on every track change.
///
/// Requests are **replaced** rather than queued: the slot holds at most one, so
/// holding `n` down cannot pile up work. A job is dropped, before and after the
/// expensive part, if a newer generation has been requested meanwhile — that is
/// the cancellation. Only one thread ever exists, no matter how fast tracks change.
pub struct CoverLoader {
    slot: Arc<(Mutex<Option<Request>>, Condvar)>,
    latest: Arc<AtomicU64>,
    results: Receiver<Loaded>,
}

impl CoverLoader {
    pub fn new() -> Self {
        let slot = Arc::new((Mutex::new(None), Condvar::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let (tx, results) = mpsc::channel();

        let worker_slot = Arc::clone(&slot);
        let worker_latest = Arc::clone(&latest);
        thread::spawn(move || {
            let (lock, cvar) = &*worker_slot;
            loop {
                let request: Request = {
                    let mut guard = lock.lock().expect("cover slot poisoned");
                    loop {
                        if let Some(request) = guard.take() {
                            break request;
                        }
                        guard = cvar.wait(guard).expect("cover slot poisoned");
                    }
                };

                // Superseded while it sat in the slot.
                if request.generation < worker_latest.load(Ordering::Acquire) {
                    continue;
                }

                let image =
                    library::load_cover(&request.path).map(|img| shrink(img, request.max_px));

                // Superseded while we were decoding; do not bother the UI with it.
                if request.generation < worker_latest.load(Ordering::Acquire) {
                    continue;
                }
                let reply = Loaded {
                    generation: request.generation,
                    image,
                };
                if tx.send(reply).is_err() {
                    return; // the app is gone
                }
            }
        });

        Self {
            slot,
            latest,
            results,
        }
    }

    /// Queue `request`, discarding any request that has not started yet.
    pub fn request(&self, request: Request) {
        // Publish the generation first, so a worker mid-job sees it and bails.
        self.latest.store(request.generation, Ordering::Release);
        let (lock, cvar) = &*self.slot;
        *lock.lock().expect("cover slot poisoned") = Some(request);
        cvar.notify_one();
    }

    /// Non-blocking: whatever the worker has finished since the last call.
    pub fn drain(&self) -> TryIter<'_, Loaded> {
        self.results.try_iter()
    }
}

impl Default for CoverLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Shrink so the longest side is at most `max_px`. Never enlarges.
fn shrink(img: DynamicImage, max_px: u32) -> DynamicImage {
    let max_px = max_px.max(1);
    if img.width().max(img.height()) <= max_px {
        return img;
    }
    img.resize(max_px, max_px, FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn jpeg(side: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(side, side, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 90])
        }));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// A folder holding one silent wav and a sidecar `cover.jpg`, which is the
    /// fallback `library::load_cover` uses when there is no embedded picture.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str, cover_side: u32) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tuneterm-cover-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cover.jpg"), jpeg(cover_side)).unwrap();
            std::fs::write(dir.join("01 song.wav"), []).unwrap();
            Self(dir)
        }

        fn track(&self) -> PathBuf {
            self.0.join("01 song.wav")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Block until the worker answers, so the test does not race it.
    fn wait_for(loader: &CoverLoader, generation: u64) -> Option<Loaded> {
        for _ in 0..200 {
            let mut found = None;
            for reply in loader.drain() {
                found = Some(reply);
            }
            if let Some(reply) = found {
                if reply.generation == generation {
                    return Some(reply);
                }
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn loads_a_cover_off_thread() {
        let fixture = Fixture::new("load", 400);
        let loader = CoverLoader::new();
        loader.request(Request {
            generation: 1,
            path: fixture.track(),
            max_px: 1000,
        });

        let reply = wait_for(&loader, 1).expect("no reply");
        let img = reply.image.expect("cover missing");
        assert_eq!(
            (img.width(), img.height()),
            (400, 400),
            "left at native size"
        );
    }

    #[test]
    fn shrinks_oversized_covers_before_handing_them_over() {
        let fixture = Fixture::new("shrink", 1200);
        let loader = CoverLoader::new();
        loader.request(Request {
            generation: 1,
            path: fixture.track(),
            max_px: 300,
        });

        let img = wait_for(&loader, 1)
            .expect("no reply")
            .image
            .expect("cover");
        assert_eq!(img.width().max(img.height()), 300, "shrunk to the cap");
    }

    /// The whole point: hammering next must not pile up work, and only the newest
    /// request may produce a reply.
    #[test]
    fn rapid_requests_collapse_to_the_last_one() {
        let fixture = Fixture::new("collapse", 900);
        let loader = CoverLoader::new();
        for generation in 1..=25 {
            loader.request(Request {
                generation,
                path: fixture.track(),
                max_px: 400,
            });
        }

        assert!(
            wait_for(&loader, 25).is_some(),
            "last generation never arrived"
        );
        // Nothing newer than the last request may show up afterwards.
        for reply in loader.drain() {
            assert!(reply.generation <= 25, "stale reply {}", reply.generation);
        }
    }

    #[test]
    fn missing_cover_reports_none_rather_than_hanging() {
        let dir = std::env::temp_dir().join(format!("tuneterm-cover-{}-empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01 song.wav"), []).unwrap();

        let loader = CoverLoader::new();
        loader.request(Request {
            generation: 7,
            path: dir.join("01 song.wav"),
            max_px: 400,
        });

        let reply = wait_for(&loader, 7).expect("no reply");
        assert!(reply.image.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrink_never_enlarges() {
        let img = DynamicImage::ImageRgb8(RgbImage::new(120, 60));
        let out = shrink(img, 4000);
        assert_eq!((out.width(), out.height()), (120, 60));
    }
}
