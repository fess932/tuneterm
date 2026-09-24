//! Stars the server keeps for its tracks.
//!
//! One hidden text file in the music folder, `stars nanos path` per line, so the
//! stars travel with the music: back it up, move the folder, and they come along.
//! Hidden, so neither the listing nor the scan ever sees it as part of the library.
//!
//! Each entry carries when it last changed, and a star taken away stays behind as
//! a `0` for the same reason: a folder's `newest` has to move when its stars do,
//! or a player that remembered the folder would never ask for it again.
//!
//! Every change writes the whole file out again. A line per rated track is tens of
//! kilobytes for thousands of them — less than one cover — and a file that is
//! only ever replaced whole cannot be left half-written.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const FILE: &str = ".tuneterm-ratings.txt";

pub struct Ratings {
    file: PathBuf,
    /// Path from the top of the music folder → stars (0 once taken away) and when.
    entries: Mutex<HashMap<String, (u8, u64)>>,
}

impl Ratings {
    /// Read what is in `root`. A missing file is no stars yet; a line that does not
    /// parse costs that line and nothing else.
    pub fn load(root: &Path) -> Self {
        let file = root.join(FILE);
        let entries = std::fs::read_to_string(&file)
            .map(|text| parse(&text))
            .unwrap_or_default();
        Self {
            file,
            entries: Mutex::new(entries),
        }
    }

    /// Stars of the track at `path`, if it has any.
    pub fn get(&self, path: &str) -> Option<u8> {
        let entries = self.entries.lock().expect("ratings poisoned");
        entries
            .get(path)
            .map(|&(stars, _)| stars)
            .filter(|&stars| stars > 0)
    }

    /// Give the track at `path` this many stars, 0 to take them away.
    pub fn set(&self, path: &str, stars: u8) -> io::Result<()> {
        let mut entries = self.entries.lock().expect("ratings poisoned");
        entries.insert(path.to_string(), (stars.min(3), now()));
        self.save(&entries)
    }

    /// When any stars at or below `folder` last changed, 0 for never.
    pub fn newest_under(&self, folder: &str) -> u64 {
        let entries = self.entries.lock().expect("ratings poisoned");
        entries
            .iter()
            .filter(|(path, _)| under(path, folder))
            .map(|(_, &(_, time))| time)
            .max()
            .unwrap_or(0)
    }

    /// Follow a file or folder that moved from `from` to `to`.
    pub fn moved(&self, from: &str, to: &str) -> io::Result<()> {
        let mut entries = self.entries.lock().expect("ratings poisoned");
        let going: Vec<String> = entries.keys().filter(|p| under(p, from)).cloned().collect();
        if going.is_empty() {
            return Ok(());
        }
        for path in going {
            let entry = entries.remove(&path).expect("just listed");
            let rest = &path[from.len()..];
            entries.insert(format!("{to}{rest}"), entry);
        }
        self.save(&entries)
    }

    /// Forget everything at or below `path`, which is gone.
    pub fn removed(&self, path: &str) -> io::Result<()> {
        let mut entries = self.entries.lock().expect("ratings poisoned");
        let before = entries.len();
        entries.retain(|p, _| !under(p, path));
        if entries.len() == before {
            return Ok(());
        }
        self.save(&entries)
    }

    /// Written aside and renamed into place, so a crash halfway leaves the old
    /// file rather than half of a new one.
    fn save(&self, entries: &HashMap<String, (u8, u64)>) -> io::Result<()> {
        let mut lines: Vec<_> = entries.iter().collect();
        lines.sort_by(|a, b| a.0.cmp(b.0));
        let mut text = String::from("# tuneterm stars — `stars nanos path`, 0 is taken away\n");
        for (path, (stars, time)) in lines {
            text.push_str(&format!("{stars} {time} {path}\n"));
        }
        let temp = self.file.with_extension("tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &self.file)
    }
}

/// True for `folder` itself and anything inside it. The empty path is the root.
fn under(path: &str, folder: &str) -> bool {
    folder.is_empty()
        || path == folder
        || path
            .strip_prefix(folder)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn parse(text: &str) -> HashMap<String, (u8, u64)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (stars, rest) = line.split_once(' ')?;
            let (time, path) = rest.split_once(' ')?;
            let stars: u8 = stars.parse().ok().filter(|&n| n <= 3)?;
            Some((path.to_string(), (stars, time.parse().ok()?)))
        })
        .collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_survive_a_restart_and_follow_a_move() {
        let dir = crate::server::TempDir::new("ratings");
        let ratings = Ratings::load(&dir.0);
        ratings.set("Artist/Early/01 a.mp3", 3).unwrap();
        ratings.set("Artist/Late/01 b.mp3", 1).unwrap();
        ratings.set("Beta/01 c.mp3", 2).unwrap();

        let ratings = Ratings::load(&dir.0);
        assert_eq!(ratings.get("Artist/Early/01 a.mp3"), Some(3));

        ratings.moved("Artist", "Band").unwrap();
        assert_eq!(ratings.get("Artist/Early/01 a.mp3"), None);
        assert_eq!(ratings.get("Band/Early/01 a.mp3"), Some(3));
        assert_eq!(ratings.get("Band/Late/01 b.mp3"), Some(1));

        ratings.removed("Band/Late").unwrap();
        assert_eq!(ratings.get("Band/Late/01 b.mp3"), None);
        assert_eq!(
            ratings.get("Beta/01 c.mp3"),
            Some(2),
            "only what was under it"
        );
    }

    /// Taking stars away still moves the folder's time: players that remember the
    /// folder have to hear about that too.
    #[test]
    fn taking_stars_away_is_a_change() {
        let dir = crate::server::TempDir::new("ratings-clear");
        let ratings = Ratings::load(&dir.0);
        ratings.set("Alpha/01.mp3", 2).unwrap();
        let given = ratings.newest_under("Alpha");
        assert!(given > 0);
        assert_eq!(
            ratings.newest_under("Alphabet"),
            0,
            "a prefix is not a folder"
        );

        std::thread::sleep(std::time::Duration::from_millis(2));
        ratings.set("Alpha/01.mp3", 0).unwrap();
        assert_eq!(ratings.get("Alpha/01.mp3"), None);
        assert!(ratings.newest_under("Alpha") > given);
    }
}
