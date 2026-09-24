use std::path::{Path, PathBuf};
use std::time::Duration;

use image::DynamicImage;
use lofty::prelude::*;

use crate::worker::Cancel;

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
#[derive(Clone)]
pub struct Folder {
    pub label: String,
    pub path: PathBuf,
    pub count: usize,
    /// Newest modification time under it, of the folders as well as the audio,
    /// in nanoseconds since the epoch. With `count`, what says whether a listing
    /// read earlier still holds; see [`Folder::stamp`].
    pub newest: u64,
}

/// What a folder looked like when its tracks were read. Unequal means read again.
///
/// A folder's own time moves when something in it is added, removed or renamed,
/// and a file's when it is written — a tag edit included. The count catches what
/// times alone can miss: a copy keeps the old file's time, and FAT and exFAT do
/// not keep a folder's time at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    count: usize,
    newest: u64,
}

impl Folder {
    pub fn stamp(&self) -> Stamp {
        Stamp {
            count: self.count,
            newest: self.newest,
        }
    }
}

#[derive(Clone)]
pub struct Track {
    /// Identity, and the file to open for a local track. Remote tracks put their
    /// URL here too, so the queue, the play marker and the cover cache all keep
    /// working on one key.
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<Duration>,
    /// Set for a remote track; `None` means `path` is a real file.
    pub url: Option<String>,
    /// Artwork to fetch, for tracks whose picture is not in a tag.
    pub art_url: Option<String>,
}

/// A path that names something on a `tuneterm serve` rather than on this disk.
///
/// A folder on a server is browsed by its address, `tuneterm://host:port/Artist`,
/// the same way a feed episode is played by its URL. So the browser, the trail
/// back up, the memo and the saved session carry remote folders with no idea that
/// they are remote; only these few functions look.
pub fn remote_url(path: &Path) -> Option<&str> {
    path.to_str().filter(|text| crate::remote::is_remote(text))
}

/// Subfolders of `dir`, here or on a server.
pub fn subdirs(dir: &Path) -> Result<Vec<Folder>, String> {
    match remote_url(dir) {
        Some(url) => crate::remote::folders(url),
        None => Ok(list_subdirs(dir)),
    }
}

/// Every track at or below `dir`, here or on a server. See [`scan_tracks_deep`].
/// A server does the walk itself in one call, so the cancel can only discard its
/// answer rather than cut it short.
pub fn tracks(dir: &Path, cancel: &Cancel) -> Result<Vec<Track>, String> {
    match remote_url(dir) {
        Some(url) => crate::remote::tracks(url),
        None => Ok(scan_tracks_deep(dir, cancel)),
    }
}

/// Immediate subdirectories of `dir` that hold audio anywhere beneath them, with a
/// recursive track count.
///
/// Only one level: the left pane is a directory browser now, not a flat index. Empty
/// branches are left out — a folder you cannot play anything from is only noise.
///
/// Deliberately does **not** read tags. This runs on every cursor move, and counting
/// files is a `read_dir` walk while reading tags is milliseconds per file.
pub fn list_subdirs(dir: &Path) -> Vec<Folder> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !is_hidden(p))
        .collect();
    subdirs.sort();

    subdirs
        .into_iter()
        .filter_map(|path| {
            let (count, newest) = count_audio(&path, MAX_DEPTH);
            (count > 0).then(|| Folder {
                label: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned()),
                path,
                count,
                newest,
            })
        })
        .collect()
}

/// How deep to look for audio. Deep enough for artist/album/disc, shallow enough
/// that a stray symlink into the filesystem cannot cost minutes.
pub const MAX_DEPTH: usize = 6;

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            let name = name.to_string_lossy();
            name.starts_with('.') || name == "$RECYCLE.BIN"
        })
        .unwrap_or(false)
}

/// Audio files at or below `dir`, and the newest time among them and the folders
/// they are in. No tags are read and no file is opened: one `stat` each, which is
/// what telling a folder from a file already cost — on Windows not even that, as
/// the listing carries the times.
fn count_audio(dir: &Path, depth: usize) -> (usize, u64) {
    let Ok(meta) = std::fs::metadata(dir) else {
        return (0, 0);
    };
    let mut newest = nanos(&meta);
    if depth == 0 {
        return (0, newest);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, newest);
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        // Following links, as `is_dir` did.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let (below, time) = count_audio(&path, depth - 1);
            count += below;
            newest = newest.max(time);
        } else if is_audio(&path) {
            count += 1;
            newest = newest.max(nanos(&meta));
        }
    }
    (count, newest)
}

fn nanos(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos() as u64)
}

/// Every audio file at or below `dir`, tags resolved, ordered by path.
///
/// This is what the right pane shows, so selecting an artist lists their whole
/// discography rather than an empty folder. Reading tags costs milliseconds per
/// file, which is why it runs on a worker rather than during a cursor move.
///
/// A thousand-file folder therefore runs for seconds, and the cursor does not wait
/// for it — hence `cancel`. Giving up returns nothing: a half-read folder is not a
/// listing, and the caller discards a cancelled answer anyway.
pub fn scan_tracks_deep(dir: &Path, cancel: &Cancel) -> Vec<Track> {
    let mut paths = Vec::new();
    collect_audio(dir, MAX_DEPTH, &mut paths, cancel);
    paths.sort();

    let mut tracks = Vec::with_capacity(paths.len());
    for path in paths {
        // Between files, not inside one: reading a single tag is milliseconds.
        if cancel.superseded() {
            return Vec::new();
        }
        tracks.push(read_track(path));
    }
    tracks
}

fn collect_audio(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, cancel: &Cancel) {
    if depth == 0 || cancel.superseded() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            subdirs.push(path);
        } else if is_audio(&path) {
            out.push(path);
        }
    }
    subdirs.sort();
    for sub in subdirs {
        collect_audio(&sub, depth - 1, out, cancel);
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

/// CP1251 for `0x80..=0xBF`. Everything from `0xC0` up is the contiguous Cyrillic
/// block and is computed instead. `\u{FFFD}` marks the one byte CP1251 leaves
/// undefined, which also serves as a "this was not CP1251" signal.
#[rustfmt::skip]
const CP1251_HIGH: [char; 64] = [
    'Ђ', 'Ѓ', '‚', 'ѓ', '„', '…', '†', '‡', '€', '‰', 'Љ', '‹', 'Њ', 'Ќ', 'Ћ', 'Џ',
    'ђ', '‘', '’', '“', '”', '•', '–', '—', '\u{FFFD}', '™', 'љ', '›', 'њ', 'ќ', 'ћ', 'џ',
    '\u{A0}', 'Ў', 'ў', 'Ј', '¤', 'Ґ', '¦', '§', 'Ё', '©', 'Є', '«', '¬', '\u{AD}', '®', 'Ї',
    '°', '±', 'І', 'і', 'ґ', 'µ', '¶', '·', 'ё', '№', 'є', '»', 'ј', 'Ѕ', 'ѕ', 'ї',
];

/// Recover a tag that holds CP1251 bytes while declaring ISO-8859-1.
///
/// Extremely common in Russian MP3s. Verified on a real file: the `TIT2` frame
/// declares encoding `0x00` (Latin-1) and carries `c2 e0 ed ff`, so a spec-correct
/// decoder yields `Âàíÿ` where the artist meant `Ваня`.
///
/// Returns `None` unless the text really looks like that mistake, because plain
/// Latin-1 titles — `Björk`, `Café`, `Motörhead` — must survive untouched. The
/// discriminator is that Cyrillic-as-Latin-1 turns *whole words* into high bytes,
/// while an accented Latin word has one or two among ASCII letters.
fn recover_cp1251(text: &str) -> Option<String> {
    let mut high = 0usize;
    let mut letters = 0usize;
    for ch in text.chars() {
        // A codepoint above 0xFF cannot have come from a single-byte decode.
        if ch as u32 > 0xFF {
            return None;
        }
        if ch.is_alphabetic() {
            letters += 1;
        }
        if (ch as u32) >= 0x80 {
            high += 1;
        }
    }
    // Two high bytes minimum, and they must outnumber the plain-ASCII letters.
    if high < 2 || high * 2 <= letters {
        return None;
    }

    let recovered: String = text
        .chars()
        .map(|ch| match ch as u32 {
            byte @ 0xC0..=0xFF => char::from_u32(0x410 + (byte - 0xC0)).unwrap_or('\u{FFFD}'),
            byte @ 0x80..=0xBF => CP1251_HIGH[(byte - 0x80) as usize],
            _ => ch,
        })
        .collect();

    if recovered.contains('\u{FFFD}') {
        return None;
    }
    // The point was to get Cyrillic; if we did not, leave the original alone.
    let cyrillic = recovered.chars().filter(|c| is_cyrillic(*c)).count();
    (cyrillic >= 2 && cyrillic * 2 > letters).then_some(recovered)
}

fn is_cyrillic(ch: char) -> bool {
    matches!(ch, '\u{0400}'..='\u{04FF}')
}

/// Apply [`recover_cp1251`] where it fires, otherwise keep the text as read.
fn fix_encoding(text: String) -> String {
    recover_cp1251(&text).unwrap_or(text)
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
        url: None,
        art_url: None,
    };

    if let Ok(tagged) = lofty::read_from_path(&track.path) {
        track.duration = Some(tagged.properties().duration());
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(v) = tag.title() {
                track.title = fix_encoding(v.into_owned());
            }
            // Some tags carry only the album artist — this album is one of them.
            let artist = tag
                .artist()
                .or_else(|| tag.get_string(ItemKey::AlbumArtist).map(Into::into));
            if let Some(v) = artist {
                track.artist = fix_encoding(v.into_owned());
            }
            if let Some(v) = tag.album() {
                track.album = fix_encoding(v.into_owned());
            }
        }
    }
    track
}

/// Raw encoded bytes of a track's cover: embedded picture first, then a sidecar
/// file in the folder.
///
/// Returned undecoded on purpose. Decoding is the expensive half, and the bytes are
/// what the cache is keyed on — every track on an album carries the same ones.
pub fn load_cover_bytes(path: &Path) -> Option<Vec<u8>> {
    if let Ok(tagged) = lofty::read_from_path(path) {
        for tag in tagged.tags() {
            if let Some(pic) = tag.pictures().first() {
                let data = pic.data();
                if !data.is_empty() {
                    return Some(data.to_vec());
                }
            }
        }
    }

    let dir = path.parent()?;
    for name in COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file()
            && let Ok(bytes) = std::fs::read(&candidate)
            && !bytes.is_empty()
        {
            return Some(bytes);
        }
    }
    None
}

/// Cover art for a track, decoded. Convenience wrapper over [`load_cover_bytes`].
pub fn load_cover(path: &Path) -> Option<DynamicImage> {
    image::load_from_memory(&load_cover_bytes(path)?).ok()
}

#[cfg(feature = "player")]
/// Turn a feed's episodes into tracks, so everything downstream — the queue, the
/// play marker, the cover pipeline — needs no idea where they came from.
pub fn tracks_from_feed(channel: &crate::feed::Channel) -> Vec<Track> {
    // A feed lists newest first. A numbered archive reads better the other way, so
    // episode 1 is row 1 and `n` walks forwards through the series.
    channel
        .episodes
        .iter()
        .rev()
        .map(|episode| Track {
            path: PathBuf::from(&episode.url),
            title: episode.title.clone(),
            artist: if episode.author.is_empty() {
                channel.title.clone()
            } else {
                episode.author.clone()
            },
            album: channel.title.clone(),
            duration: episode.duration,
            url: Some(episode.url.clone()),
            art_url: episode.art_url.clone(),
        })
        .collect()
}

pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real case: an ID3v2.3 frame declaring encoding 0x00 (Latin-1) while
    /// carrying CP1251 bytes. Verified against an actual file whose TIT2 holds
    /// `c2 e0 ed ff`.
    #[test]
    fn recovers_cyrillic_stored_as_latin1() {
        for (broken, want) in [
            ("Âàíÿ", "Ваня"),
            ("Ñàøà", "Саша"),
            ("Ïàïà", "Папа"),
            ("Äåòè", "Дети"),
            ("Àôôèíàæ", "Аффинаж"),
            ("Ðóññêèå ïåñíè", "Русские песни"),
        ] {
            assert_eq!(recover_cp1251(broken).as_deref(), Some(want), "{broken}");
        }
    }

    /// Text that is genuinely Latin-1 must survive untouched, or fixing Russian
    /// tags would break every accented Western name.
    #[test]
    fn leaves_real_latin1_alone() {
        for text in [
            "Björk",
            "Café",
            "Motörhead",
            "Sigur Rós",
            "Éléphant",
            "Mötley Crüe",
            "Blue Öyster Cult",
            "Naïve",
            "Sinéad O'Connor",
            "Zoë",
        ] {
            assert_eq!(recover_cp1251(text), None, "{text} was mangled");
        }
    }

    #[test]
    fn leaves_plain_ascii_and_real_unicode_alone() {
        for text in ["Show Me", "", "1979", "Ваня", "日本語", "Ελλάδα"] {
            assert_eq!(recover_cp1251(text), None, "{text}");
        }
    }

    /// A single high byte is an accent, not a Cyrillic word.
    #[test]
    fn needs_more_than_one_high_byte() {
        assert_eq!(recover_cp1251("Ä"), None);
        assert_eq!(recover_cp1251("aÄa"), None);
    }

    /// Punctuation and digits around the letters must not defeat the heuristic.
    #[test]
    fn survives_mixed_punctuation() {
        assert_eq!(
            recover_cp1251("Ëåòàþ_Ðàñòó").as_deref(),
            Some("Летаю_Расту")
        );
        assert_eq!(
            recover_cp1251("01. Âàíÿ (2013)").as_deref(),
            Some("01. Ваня (2013)")
        );
    }

    /// Reading tags is the slow half, and a cancelled scan must not sit through it.
    #[test]
    fn a_cancelled_scan_reads_nothing() {
        let dir = std::env::temp_dir().join(format!("tuneterm-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..=3 {
            std::fs::write(dir.join(format!("{i:02}.wav")), []).unwrap();
        }

        assert_eq!(scan_tracks_deep(&dir, &Cancel::never()).len(), 3);
        assert!(
            scan_tracks_deep(&dir, &Cancel::already()).is_empty(),
            "a cancelled scan still produced a listing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_encoding_is_the_identity_when_nothing_is_wrong() {
        assert_eq!(fix_encoding("Björk".into()), "Björk");
        assert_eq!(fix_encoding("Ваня".into()), "Ваня");
        assert_eq!(fix_encoding("Âàíÿ".into()), "Ваня");
    }
}
