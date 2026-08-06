//! On-disk cache for scaled cover art.
//!
//! Every track on an album usually carries the same picture, so without this the
//! same 1400 px JPEG gets decoded and scaled again for each track. A cache entry
//! is keyed by the *bytes of the source picture* rather than by the track path, so
//! all tracks of an album — and re-runs of the app — share one entry.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::DynamicImage;

/// Total budget for the cache directory. Entries are evicted oldest-first.
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Platform cache directory, `tuneterm` subfolder. `None` if there is no home.
///
/// `TUNETERM_CACHE_DIR` overrides it, which is how the tests avoid writing into the
/// real user cache.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("TUNETERM_CACHE_DIR") {
        Some(over) if !over.is_empty() => Some(PathBuf::from(over)),
        _ => platform_dir(),
    }
}

/// Where the cache goes when nothing overrides it.
fn platform_dir() -> Option<PathBuf> {
    // Deliberately hand-rolled: one small function beats a dependency, and the
    // rules are short.
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        match std::env::var_os("XDG_CACHE_HOME") {
            Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
        }
    };
    Some(base.join("tuneterm"))
}

/// Stable 64-bit content key. FNV-1a: no dependency, and a cache miss is the only
/// consequence of a collision this unlikely.
pub fn key(picture: &[u8], box_px: (u32, u32)) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    eat(picture);
    eat(&picture.len().to_le_bytes());
    // The stored file is already scaled, so the target size is part of the key.
    eat(&box_px.0.to_le_bytes());
    eat(&box_px.1.to_le_bytes());

    format!("{hash:016x}-{}x{}.png", box_px.0, box_px.1)
}

/// Where an entry lives, whether or not it exists yet.
pub fn path(key: &str) -> Option<PathBuf> {
    Some(dir()?.join(key))
}

/// A previously scaled cover, if it is on disk.
pub fn get(key: &str) -> Option<DynamicImage> {
    get_in(&dir()?, key)
}

pub fn get_in(dir: &Path, key: &str) -> Option<DynamicImage> {
    let path = dir.join(key);
    let img = image::open(&path).ok()?;
    // Touch it so the sweep treats it as recently used.
    let _ = fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(SystemTime::now()));
    Some(img)
}

/// Store a scaled cover, then bring the directory back under [`MAX_BYTES`].
///
/// Every failure here is ignored on purpose: a cache that cannot be written is a
/// slower app, not a broken one.
pub fn put(key: &str, img: &DynamicImage) {
    let Some(dir) = dir() else { return };
    put_in(&dir, key, img);
}

pub fn put_in(dir: &Path, key: &str, img: &DynamicImage) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(key);

    // Write to a temporary name first so a crash cannot leave a truncated PNG
    // that would later fail to decode.
    let temp = dir.join(format!("{key}.tmp{}", std::process::id()));
    if img
        .save_with_format(&temp, image::ImageFormat::Png)
        .is_err()
    {
        let _ = fs::remove_file(&temp);
        return;
    }
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }

    sweep(dir, MAX_BYTES);
}

/// Delete oldest-first until the directory fits in `max_bytes`.
pub fn sweep(dir: &Path, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, meta.len(), entry.path()))
        })
        .collect();

    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= max_bytes {
        return;
    }

    // Oldest first.
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};
    use std::time::Duration;

    fn image(side: u32, tint: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(side, side, image::Rgb([tint, 20, 30])))
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("tuneterm-cache-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dir_lands_under_a_platform_cache_path() {
        let dir = platform_dir().expect("no cache dir");
        assert!(dir.ends_with("tuneterm"), "{dir:?}");
        if cfg!(target_os = "macos") {
            assert!(dir.to_string_lossy().contains("Library/Caches"), "{dir:?}");
        }
    }

    /// The whole point: the same picture at the same size is one entry, whatever
    /// track it came from.
    #[test]
    fn identical_pictures_share_a_key() {
        let picture = b"pretend this is a jpeg";
        assert_eq!(key(picture, (300, 300)), key(picture, (300, 300)));
        assert_ne!(key(picture, (300, 300)), key(picture, (600, 600)));
        assert_ne!(
            key(picture, (300, 300)),
            key(b"a different cover", (300, 300))
        );
    }

    #[test]
    fn key_is_a_usable_filename() {
        let key = key(b"x", (128, 64));
        assert!(key.ends_with("-128x64.png"), "{key}");
        assert!(
            !key.contains('/') && !key.contains('\\') && !key.contains(char::is_whitespace),
            "{key}"
        );
    }

    /// Store then fetch, which is the path `cover.rs` relies on to skip decoding a
    /// 1400 px original for every track of an album.
    #[test]
    fn roundtrips_a_scaled_cover() {
        let temp = TempDir::new("roundtrip");
        let key = key(b"album art bytes", (300, 300));

        assert!(get_in(&temp.0, &key).is_none(), "empty cache must miss");

        put_in(&temp.0, &key, &image(300, 90));
        let got = get_in(&temp.0, &key).expect("cache miss after put");
        assert_eq!((got.width(), got.height()), (300, 300));
        assert!(temp.0.join(&key).is_file(), "entry not on disk");
    }

    /// A half-written entry must not be served. `put_in` writes to a temp name and
    /// renames, so a truncated file can never carry the real key.
    #[test]
    fn a_corrupt_entry_is_a_miss_not_a_crash() {
        let temp = TempDir::new("corrupt");
        let key = key(b"x", (64, 64));
        fs::write(temp.0.join(&key), b"not a png").unwrap();
        assert!(get_in(&temp.0, &key).is_none());
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let temp = TempDir::new("notemp");
        put_in(&temp.0, &key(b"y", (64, 64)), &image(64, 5));
        let temps: Vec<_> = fs::read_dir(&temp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(temps.is_empty(), "{temps:?}");
    }

    #[test]
    fn sweep_keeps_the_newest_and_drops_the_oldest() {
        let temp = TempDir::new("sweep");
        // Four 64x64 PNGs, written oldest to newest.
        let mut paths = Vec::new();
        for i in 0..4u8 {
            let path = temp.0.join(format!("{i}.png"));
            image(64, i * 40)
                .save_with_format(&path, image::ImageFormat::Png)
                .unwrap();
            let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + u64::from(i) * 60);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(stamp)
                .unwrap();
            paths.push(path);
        }

        let total: u64 = paths.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
        // Allow room for roughly half of them.
        sweep(&temp.0, total / 2);

        let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert!(!left[0], "oldest should be gone");
        assert!(left[3], "newest should survive");
        let remaining: u64 = paths
            .iter()
            .filter(|p| p.exists())
            .map(|p| fs::metadata(p).unwrap().len())
            .sum();
        assert!(remaining <= total / 2, "still over budget: {remaining}");
    }

    #[test]
    fn sweep_leaves_a_directory_under_budget_alone() {
        let temp = TempDir::new("under");
        let path = temp.0.join("only.png");
        image(32, 7)
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        sweep(&temp.0, MAX_BYTES);
        assert!(path.exists());
    }

    #[test]
    fn sweep_on_a_missing_directory_is_harmless() {
        sweep(Path::new("/nonexistent/tuneterm-cache"), 1);
    }
}
