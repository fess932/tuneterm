use std::path::{Path, PathBuf};

use image::DynamicImage;
use image::imageops::FilterType;

use crate::cache;
use crate::library;
use crate::worker::Worker;

pub struct Request {
    pub generation: u64,
    pub path: PathBuf,
    /// Artwork to download, for tracks that carry no embedded picture.
    pub art_url: Option<String>,
    /// The pixel box the art will occupy. The worker returns an image scaled to
    /// fill it — up or down — so the render thread never resizes anything.
    pub box_px: (u32, u32),
}

pub struct Loaded {
    pub generation: u64,
    /// `None` means the track simply has no cover.
    pub image: Option<DynamicImage>,
    /// Where the scaled cover sits in the cache, for the OS "now playing" artwork.
    pub file: Option<PathBuf>,
}

/// Loads and scales cover art off the render thread.
///
/// Decoding a 1400 px JPEG costs ~190 ms and scaling it ~400 ms in a debug build,
/// which would stall the UI on every track change. Cancellation and the guarantee
/// of a single thread come from [`Worker`].
pub struct CoverLoader {
    worker: Worker<Request, (Option<DynamicImage>, Option<PathBuf>)>,
}

impl CoverLoader {
    pub fn new() -> Self {
        Self::with_cache(cache::dir_for(cache::Kind::Art))
    }

    /// Explicit cache directory. Tests use it to stay out of the real one, which
    /// also stops them from depending on whatever is already in it.
    pub fn with_cache(cache_dir: Option<PathBuf>) -> Self {
        Self {
            worker: Worker::spawn("covers", move |request: Request| {
                prepare(
                    &request.path,
                    request.art_url.as_deref(),
                    request.box_px,
                    cache_dir.as_deref(),
                )
            }),
        }
    }

    /// Queue `request`, discarding one that has not started yet.
    pub fn request(&self, request: Request) {
        self.worker.request(request.generation, request);
    }

    /// Non-blocking: whatever the worker has finished since the last call.
    pub fn drain(&self) -> impl Iterator<Item = Loaded> + '_ {
        self.worker
            .drain()
            .map(|(generation, (image, file))| Loaded {
                generation,
                image,
                file,
            })
    }
}

impl Default for CoverLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode and scale a track's cover to fill `box_px`, using the disk cache.
///
/// Ordered so the cheap steps come first: reading the tag is ~15 ms, hashing the
/// picture ~0.2 ms, and decoding a cached 300 px PNG ~13 ms — against ~190 ms to
/// decode the 1400 px original plus ~400 ms to scale it.
fn prepare(
    path: &Path,
    art_url: Option<&str>,
    box_px: (u32, u32),
    cache_dir: Option<&Path>,
) -> (Option<DynamicImage>, Option<PathBuf>) {
    // A remote track has no tag to read; its artwork is a URL. Downloading it is
    // exactly as slow as decoding a large JPEG, and this is the same worker, so it
    // costs the interface nothing either way.
    let picture = match art_url {
        Some(url) => crate::net::get(url).ok(),
        None => library::load_cover_bytes(path),
    };
    let Some(picture) = picture else {
        return (None, None);
    };
    let key = cache::key(&picture, box_px);
    let file = cache_dir.map(|dir| dir.join(&key));

    if let Some(cached) = cache_dir.and_then(|dir| cache::get_in(dir, &key)) {
        return (Some(cached), file);
    }

    let Ok(decoded) = image::load_from_memory(&picture) else {
        return (None, None);
    };
    let scaled = fill(decoded, box_px);
    if let Some(dir) = cache_dir {
        cache::put_in(dir, &key, &scaled);
    }
    // Only advertise the file if it really landed.
    let file = file.filter(|path| path.is_file());
    (Some(scaled), file)
}

/// Scale to fill `box_px` as far as the aspect ratio allows, enlarging if needed.
///
/// Enlarging is fine here — this runs off the render thread, and a cover that only
/// covered 60% of its pane looked like a bug. Lanczos3 is affordable for the same
/// reason, and it matters most when upscaling.
fn fill(img: DynamicImage, box_px: (u32, u32)) -> DynamicImage {
    let (box_w, box_h) = (box_px.0.max(1), box_px.1.max(1));
    if img.width() == 0 || img.height() == 0 {
        return img;
    }
    // `resize` already fits within the box and preserves the aspect ratio; it just
    // will not enlarge, so do that case by hand.
    if img.width() >= box_w || img.height() >= box_h {
        return img.resize(box_w, box_h, FilterType::Lanczos3);
    }
    let scale = f64::from(box_w) / f64::from(img.width());
    let scale = scale.min(f64::from(box_h) / f64::from(img.height()));
    let width = (f64::from(img.width()) * scale).round().max(1.0) as u32;
    let height = (f64::from(img.height()) * scale).round().max(1.0) as u32;
    img.resize_exact(width, height, FilterType::Lanczos3)
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

        /// A loader whose cache lives inside this fixture, so nothing is shared.
        fn loader(&self) -> CoverLoader {
            CoverLoader::with_cache(Some(self.0.join("cache")))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Block until the worker answers, so the test does not race it.
    fn wait_for(loader: &CoverLoader, generation: u64) -> Option<Loaded> {
        for _ in 0..800 {
            let mut found = None;
            for reply in loader.drain() {
                found = Some(reply);
            }
            if let Some(reply) = found
                && reply.generation == generation
            {
                return Some(reply);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn loads_a_cover_off_thread() {
        let fixture = Fixture::new("load", 400);
        let loader = fixture.loader();
        loader.request(Request {
            generation: 1,
            path: fixture.track(),
            art_url: None,
            box_px: (1000, 1000),
        });

        let reply = wait_for(&loader, 1).expect("no reply");
        let img = reply.image.expect("cover missing");
        assert_eq!(
            (img.width(), img.height()),
            (1000, 1000),
            "a 400px cover is enlarged to fill the box"
        );
    }

    #[test]
    fn shrinks_oversized_covers_before_handing_them_over() {
        let fixture = Fixture::new("shrink", 1200);
        let loader = fixture.loader();
        loader.request(Request {
            generation: 1,
            path: fixture.track(),
            art_url: None,
            box_px: (300, 300),
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
        let loader = fixture.loader();
        for generation in 1..=25 {
            loader.request(Request {
                generation,
                path: fixture.track(),
                art_url: None,
                box_px: (400, 400),
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
            art_url: None,
            box_px: (400, 400),
        });

        let reply = wait_for(&loader, 7).expect("no reply");
        assert!(reply.image.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Filling must keep the aspect ratio and touch at least one edge of the box.
    #[test]
    fn fill_touches_an_edge_and_keeps_the_aspect_ratio() {
        for (src, box_px, want) in [
            ((120u32, 60u32), (600u32, 600u32), (600u32, 300u32)), // enlarged, width-bound
            ((60, 120), (600, 600), (300, 600)),                   // enlarged, height-bound
            ((1200, 1200), (300, 300), (300, 300)),                // shrunk, square
            ((2000, 1000), (300, 300), (300, 150)),                // shrunk, width-bound
            ((300, 300), (300, 300), (300, 300)),                  // already exact
        ] {
            let img = DynamicImage::ImageRgb8(RgbImage::new(src.0, src.1));
            let out = fill(img, box_px);
            assert_eq!((out.width(), out.height()), want, "{src:?} into {box_px:?}");
            assert!(
                out.width() == box_px.0 || out.height() == box_px.1,
                "{src:?} into {box_px:?} touched neither edge: {out:?}",
                out = (out.width(), out.height())
            );
        }
    }

    #[test]
    fn fill_survives_a_degenerate_box() {
        let img = DynamicImage::ImageRgb8(RgbImage::new(10, 10));
        let out = fill(img, (0, 0));
        assert!(out.width() >= 1 && out.height() >= 1);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// `TUNETERM_CACHE_DIR=/tmp/tt-bench cargo test bench_cache -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_cache() {
        use image::{DynamicImage, RgbImage};
        let dir = std::env::temp_dir().join(format!("tuneterm-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        println!(
            "cache dir: {:?}   profile: {}",
            cache::dir_for(cache::Kind::Art),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );

        for side in [300u32, 1400] {
            let img = DynamicImage::ImageRgb8(RgbImage::from_fn(side, side, |x, y| {
                image::Rgb([(x % 255) as u8, (y % 255) as u8, 90])
            }));
            let mut jpeg = std::io::Cursor::new(Vec::new());
            img.write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
            std::fs::write(dir.join("cover.jpg"), jpeg.into_inner()).unwrap();
            // Two "tracks" of the same album share the picture.
            std::fs::write(dir.join("01.wav"), []).unwrap();
            std::fs::write(dir.join("02.wav"), []).unwrap();

            let box_px = (600, 600);
            let t = Instant::now();
            let cache_dir = cache::dir_for(cache::Kind::Art);
            let (first, _) = prepare(&dir.join("01.wav"), None, box_px, cache_dir.as_deref());
            let cold = t.elapsed();

            let t = Instant::now();
            let (second, _) = prepare(&dir.join("02.wav"), None, box_px, cache_dir.as_deref());
            let warm = t.elapsed();

            println!(
                "  {side:>4}px source -> {:?}   cold {:>8.1?}   warm {:>8.1?}",
                first.map(|i| (i.width(), i.height())),
                cold,
                warm
            );
            assert!(second.is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
