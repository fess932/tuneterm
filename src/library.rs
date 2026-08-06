use std::path::{Path, PathBuf};
use std::time::Duration;

use image::DynamicImage;
use lofty::prelude::*;

const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "oga", "opus", "wav", "aiff", "wv",
];

const COVER_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "front.jpg",
    "album.jpg",
    "AlbumArt.jpg",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// A directory that directly contains audio files.
pub struct Folder {
    pub label: String,
    pub path: PathBuf,
    pub count: usize,
}

pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<Duration>,
}

/// Walk `root` and collect every directory that directly holds audio files.
pub fn scan_folders(root: &Path, max_depth: usize) -> Vec<Folder> {
    let mut out = Vec::new();
    walk(root, root, 0, max_depth, &mut out);
    // Sort on the path, not the label: labels are abbreviated and would scatter
    // sibling folders.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<Folder>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut audio_here = 0usize;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        // skip dotfiles and Windows/macOS junk
        if name.to_string_lossy().starts_with('.') || name == "$RECYCLE.BIN" {
            continue;
        }
        if path.is_dir() {
            subdirs.push(path);
        } else if is_audio(&path) {
            audio_here += 1;
        }
    }

    if audio_here > 0 {
        let label = dir
            .strip_prefix(root)
            .ok()
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(|rel| {
                // Deep trees would blow past the pane width, and the useful part is
                // the tail (…/artist/album), so keep only the last two components.
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                let tail = parts[parts.len().saturating_sub(2)..].join(" / ");
                if parts.len() > 2 {
                    format!("… / {tail}")
                } else {
                    tail
                }
            })
            // The root itself has an empty relative path; use its own name.
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.to_string_lossy().into_owned())
            });
        out.push(Folder {
            label,
            path: dir.to_path_buf(),
            count: audio_here,
        });
    }

    subdirs.sort();
    for sub in subdirs {
        walk(root, &sub, depth + 1, max_depth, out);
    }
}

/// Read every audio file directly inside `dir`, with tags resolved.
pub fn scan_tracks(dir: &Path) -> Vec<Track> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_audio(p))
        .collect();
    paths.sort();
    paths.into_iter().map(read_track).collect()
}

fn read_track(path: PathBuf) -> Track {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".into());

    let mut track = Track {
        title: stem,
        artist: "—".into(),
        album: "—".into(),
        duration: None,
        path,
    };

    if let Ok(tagged) = lofty::read_from_path(&track.path) {
        track.duration = Some(tagged.properties().duration());
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(v) = tag.title() {
                track.title = v.into_owned();
            }
            if let Some(v) = tag.artist() {
                track.artist = v.into_owned();
            }
            if let Some(v) = tag.album() {
                track.album = v.into_owned();
            }
        }
    }
    track
}

/// Cover art for a track: embedded picture first, then a sidecar file in the folder.
pub fn load_cover(path: &Path) -> Option<DynamicImage> {
    if let Ok(tagged) = lofty::read_from_path(path) {
        for tag in tagged.tags() {
            if let Some(pic) = tag.pictures().first()
                && let Ok(img) = image::load_from_memory(pic.data())
            {
                return Some(img);
            }
        }
    }

    let dir = path.parent()?;
    for name in COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file()
            && let Ok(img) = image::open(&candidate)
        {
            return Some(img);
        }
    }
    None
}

pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}
