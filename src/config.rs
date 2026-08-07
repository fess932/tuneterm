//! The user's list of feeds, on disk.
//!
//! Plain text, one entry per line, `#` for comments, and an optional `name = url`
//! when a feed's own title is unhelpful:
//!
//! ```text
//! # mixtapes
//! Music For Programming = https://musicforprogramming.net/rss.xml
//! https://example.com/podcast.xml
//! ```
//!
//! Text rather than TOML or JSON for the same reason the cache paths are
//! hand-rolled: no dependency, obvious in an editor, and a bad line costs one entry
//! instead of the whole file.

use std::fs;
use std::path::{Path, PathBuf};

/// Shipped so the tab is not empty on a first run — and it is the feed that
/// prompted all of this.
pub const DEFAULT_FEED: (&str, &str) = (
    "Music For Programming",
    "https://musicforprogramming.net/rss.xml",
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    /// What to show. Falls back to the host when the line carries no name.
    pub name: String,
    pub url: String,
}

/// Config directory, `TUNETERM_CONFIG_DIR` overriding it. Separate from the cache:
/// this is the user's list, not something we can regenerate.
pub fn dir() -> Option<PathBuf> {
    if let Some(over) = std::env::var_os("TUNETERM_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(over));
    }
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        }
    };
    Some(base.join("tuneterm"))
}

pub fn feeds_path() -> Option<PathBuf> {
    Some(dir()?.join("feeds.txt"))
}

/// Read the list, falling back to the default when there is no file yet.
pub fn load_feeds() -> Vec<Feed> {
    let text = feeds_path().and_then(|path| fs::read_to_string(path).ok());
    match text {
        Some(text) => parse(&text),
        None => vec![default_feed()],
    }
}

pub fn default_feed() -> Feed {
    Feed {
        name: DEFAULT_FEED.0.to_string(),
        url: DEFAULT_FEED.1.to_string(),
    }
}

/// Write the list to an explicit path. `Err` carries something worth putting in the
/// status line. The app holds the path rather than looking it up, so tests never
/// touch the real file.
pub fn save_feeds_to(path: &Path, feeds: &[Feed]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }

    let mut text = String::from("# tuneterm feeds — one per line, `name = url`, `#` comments\n");
    for feed in feeds {
        if feed.name.is_empty() || feed.name == host_of(&feed.url) {
            text.push_str(&feed.url);
        } else {
            text.push_str(&format!("{} = {}", feed.name, feed.url));
        }
        text.push('\n');
    }
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read a list from an explicit path. Used by the tests to check what was written.
#[cfg_attr(not(test), allow(dead_code))]
pub fn load_feeds_from(path: &Path) -> Vec<Feed> {
    fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

fn parse(text: &str) -> Vec<Feed> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            // `name = url`, but only when the part after `=` still looks like a URL:
            // a bare URL can contain `=` in its query string.
            let (name, url) = match line.split_once('=') {
                Some((name, rest)) if is_url(rest.trim()) => (name.trim().to_string(), rest.trim()),
                _ => (String::new(), line),
            };
            if !is_url(url) {
                return None;
            }
            Some(Feed {
                name: if name.is_empty() { host_of(url) } else { name },
                url: url.to_string(),
            })
        })
        .collect()
}

/// Deliberately shallow: enough to reject a typo, not a validator.
pub fn is_url(text: &str) -> bool {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    rest.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
}

/// Host of a URL, for naming a feed that arrived without one.
pub fn host_of(url: &str) -> String {
    url.split_once("//")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_is_separate_from_the_cache() {
        let dir = dir().expect("no config dir");
        assert!(dir.ends_with("tuneterm"), "{dir:?}");
        if cfg!(target_os = "macos") {
            assert!(
                dir.to_string_lossy().contains("Application Support"),
                "{dir:?}"
            );
        }
        assert!(
            !dir.to_string_lossy().contains("Caches"),
            "config must not live in the cache: {dir:?}"
        );
    }

    #[test]
    fn parses_names_comments_and_blanks() {
        let feeds = parse(
            "# a comment\n\
             \n\
             Music For Programming = https://musicforprogramming.net/rss.xml\n\
             https://example.com/podcast.xml\n\
                # indented comment\n",
        );
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].name, "Music For Programming");
        assert_eq!(feeds[0].url, "https://musicforprogramming.net/rss.xml");
        assert_eq!(feeds[1].name, "example.com", "named after its host");
    }

    /// A query string with `=` in it must not be mistaken for `name = url`.
    #[test]
    fn a_url_containing_an_equals_sign_survives() {
        let feeds = parse("https://example.com/feed?format=rss&id=7\n");
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].url, "https://example.com/feed?format=rss&id=7");
        assert_eq!(feeds[0].name, "example.com");
    }

    #[test]
    fn rejects_lines_that_are_not_urls() {
        assert!(parse("not a url\nftp://example.com/x\nhttps://\n").is_empty());
    }

    #[test]
    fn url_check_accepts_what_it_should_and_no_more() {
        for good in [
            "https://a.example/rss.xml",
            "http://a.example",
            "https://a.example/x?y=1",
        ] {
            assert!(is_url(good), "{good}");
        }
        for bad in [
            "",
            "a.example",
            "https://",
            "https:///x",
            "file:///x",
            "//x",
        ] {
            assert!(!is_url(bad), "{bad}");
        }
    }

    #[test]
    fn host_is_trimmed_for_display() {
        assert_eq!(host_of("https://www.example.com/a/b?c=1"), "example.com");
        assert_eq!(host_of("http://example.com"), "example.com");
    }

    /// Round-tripping must not lose names, and must not invent them either.
    #[test]
    fn saving_and_loading_preserves_the_list() {
        let dir = std::env::temp_dir().join(format!("tuneterm-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feeds.txt");

        let feeds = vec![
            default_feed(),
            Feed {
                name: "example.com".into(),
                url: "https://example.com/p.xml".into(),
            },
        ];
        save_feeds_to(&path, &feeds).expect("save");
        assert_eq!(load_feeds_from(&path), feeds);
        let _ = fs::remove_dir_all(&dir);
    }
}
