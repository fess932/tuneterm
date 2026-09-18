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
//!
//! `settings.txt` sits beside it in the same shape, for what the app remembers
//! about itself rather than what the user curated: the volume, and enough of the
//! last session to put you back where you left off.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

pub fn settings_path() -> Option<PathBuf> {
    Some(dir()?.join("settings.txt"))
}

/// The knobs the app sets for itself, as opposed to the list the user curates.
///
/// Same plain-text `key = value` shape as the feeds, and for the same reasons.
/// An unrecognised key is skipped rather than failing the file, so a settings
/// file written by a newer build still loads here, and so does one written before
/// any of the session keys existed — every one of them is optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub volume: Volume,
    pub shuffle: bool,
    /// The session, in as much detail as it can be put back: which source was
    /// showing, where in it you were, and what was playing.
    pub session: Session,
    /// A `tuneterm serve` to talk to. Set by hand, never by the app — which only
    /// has to carry it through its own writes.
    pub remote: Remote,
}

/// The server `push`, `ls`, `mv` and `rm` use when not given one, and the token for
/// any server whose address does not carry its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Remote {
    pub server: Option<String>,
    pub token: Option<String>,
}

/// Where the app was when it was last closed.
///
/// Everything is optional and everything is a hint: a folder can be deleted and a
/// feed can be removed between runs, so nothing here may be trusted to still
/// exist. The restoring end checks; this end only records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// Which source tab was showing: `local`, `feeds` or `radio`.
    pub tab: Option<String>,
    /// The folder the left pane was listing.
    pub folder: Option<PathBuf>,
    /// The row highlighted in it, whose tracks the right pane was showing. Often
    /// a subfolder of `folder`, and `folder` itself when it has no subfolders.
    pub selected: Option<PathBuf>,
    /// URL of the feed that was selected on the Feeds tab.
    pub feed: Option<String>,
    /// What was playing, identified the way a `Track` is: a path, or a URL for a
    /// stream.
    pub track: Option<String>,
    /// How far into it.
    pub position: Duration,
    /// Whether it was playing rather than paused, so it comes back the way it was
    /// left rather than always one way.
    pub playing: bool,
}

/// rodio's scale, kept in range by construction: 1.0 is the track untouched.
///
/// A newtype because this value is read from a file anyone can edit and then
/// handed straight to an amplifier, and "did this one get validated?" is not a
/// question worth asking at each of the places it passes through.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume(f32);

/// Matches the ceiling `AudioPlayer` clamps to, so a hand-edited file cannot ask
/// for more gain than the `+` key can.
pub const MAX_VOLUME: f32 = 2.0;

impl Volume {
    /// Anything that is not a usable number becomes the default rather than
    /// silence: a broken settings file should not look like broken audio.
    pub fn new(value: f32) -> Self {
        if value.is_finite() {
            Self(value.clamp(0.0, MAX_VOLUME))
        } else {
            Self::default()
        }
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

impl Default for Volume {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Read the settings, falling back to the defaults when there is no file yet.
pub fn load_settings() -> Settings {
    settings_path()
        .map(|path| load_settings_from(&path))
        .unwrap_or_default()
}

pub fn load_settings_from(path: &Path) -> Settings {
    fs::read_to_string(path)
        .map(|text| parse_settings(&text))
        .unwrap_or_default()
}

/// Write the settings to an explicit path. As with the feeds, the app holds the
/// path rather than looking it up, so tests never touch the real file.
pub fn save_settings_to(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut text = String::from("# tuneterm settings — written by the app, safe to edit\n");
    text.push_str(&format!("volume = {:.3}\n", settings.volume.get()));
    text.push_str(&format!("shuffle = {}\n", u8::from(settings.shuffle)));

    let remote = &settings.remote;
    text.push_str(
        "\n# a tuneterm server: `server` for push, ls, mv and rm; `token` for any server\n",
    );
    for (key, value) in [("server", &remote.server), ("token", &remote.token)] {
        if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
            text.push_str(&format!("{key} = {value}\n"));
        }
    }

    let session = &settings.session;
    text.push_str("\n# where the last session left off\n");
    for (key, value) in [
        ("tab", session.tab.clone()),
        ("folder", path_value(session.folder.as_deref())),
        ("selected", path_value(session.selected.as_deref())),
        ("feed", session.feed.clone()),
        ("track", session.track.clone()),
    ] {
        // An absent value is left out rather than written empty: the file is meant
        // to be read by a person, and a column of bare `=` says nothing.
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            text.push_str(&format!("{key} = {value}\n"));
        }
    }
    if session.position > Duration::ZERO {
        text.push_str(&format!(
            "position = {:.1}\n",
            session.position.as_secs_f64()
        ));
    }
    // Only alongside a track: on its own it would describe nothing.
    if session.track.is_some() {
        text.push_str(&format!("playing = {}\n", u8::from(session.playing)));
    }

    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// A path as it goes into the file.
///
/// Lossy, so a path that is not valid UTF-8 is written mangled and simply fails to
/// match anything on the way back in — which is the same outcome as a folder that
/// has been deleted, and is already handled.
fn path_value(path: Option<&Path>) -> Option<String> {
    Some(path?.to_string_lossy().into_owned())
}

/// The ways a person might write "on" in a file they edited by hand.
fn is_yes(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes" | "on")
}

fn parse_settings(text: &str) -> Settings {
    let mut settings = Settings::default();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        // Only the first `=` splits: a path or a URL may well contain more.
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        // An empty value means "not set", which is what a default already says.
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            // A hand-edited file is not to be trusted: a NaN or a negative would
            // otherwise reach rodio's amplifier and take the audio with it.
            "volume" => {
                if let Ok(volume) = value.parse::<f32>() {
                    settings.volume = Volume::new(volume);
                }
            }
            "shuffle" => settings.shuffle = is_yes(value),
            "playing" => settings.session.playing = is_yes(value),
            "tab" => settings.session.tab = Some(value.to_ascii_lowercase()),
            "folder" => settings.session.folder = Some(PathBuf::from(value)),
            "selected" => settings.session.selected = Some(PathBuf::from(value)),
            "feed" => settings.session.feed = Some(value.to_string()),
            "track" => settings.session.track = Some(value.to_string()),
            "server" => settings.remote.server = Some(value.to_string()),
            "token" => settings.remote.token = Some(value.to_string()),
            "position" => {
                if let Ok(secs) = value.parse::<f64>()
                    && secs.is_finite()
                    && secs >= 0.0
                {
                    settings.session.position = Duration::from_secs_f64(secs);
                }
            }
            _ => {}
        }
    }
    settings
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

    #[test]
    fn settings_default_when_there_is_no_file() {
        let missing = std::env::temp_dir().join("tuneterm-no-such-settings.txt");
        let _ = fs::remove_file(&missing);
        assert_eq!(load_settings_from(&missing), Settings::default());
        let fresh = Settings::default();
        assert_eq!(fresh.volume.get(), 1.0, "silence is not a default");
        assert!(!fresh.shuffle);
        assert_eq!(fresh.session, Session::default());
    }

    fn a_session() -> Settings {
        Settings {
            volume: Volume::new(0.35),
            shuffle: true,
            session: Session {
                tab: Some("feeds".into()),
                folder: Some(PathBuf::from("/music/Deep Purple")),
                selected: Some(PathBuf::from("/music/Deep Purple/=1")),
                feed: Some("https://example.com/feed?format=rss&id=7".into()),
                track: Some("https://example.com/ep 12.mp3".into()),
                position: Duration::from_secs_f64(93.4),
                playing: true,
            },
            remote: Remote {
                server: Some("tuneterm://nas".into()),
                token: Some("s3cret=with=equals".into()),
            },
        }
    }

    /// What the file actually looks like. It is meant to be opened in an editor,
    /// so its shape is part of the contract, not an implementation detail.
    #[test]
    fn the_written_file_is_readable() {
        let dir = std::env::temp_dir().join(format!("tuneterm-set-shape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("settings.txt");
        save_settings_to(&path, &a_session()).expect("save");

        let text = fs::read_to_string(&path).expect("read");
        for line in [
            "volume = 0.350",
            "shuffle = 1",
            "tab = feeds",
            "folder = /music/Deep Purple",
            "selected = /music/Deep Purple/=1",
            "track = https://example.com/ep 12.mp3",
            "position = 93.4",
            "playing = 1",
            "server = tuneterm://nas",
            "token = s3cret=with=equals",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("tuneterm-set-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("settings.txt");

        let settings = a_session();
        save_settings_to(&path, &settings).expect("save");
        assert_eq!(load_settings_from(&path), settings);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A path or a URL may contain `=`, and both are written unquoted.
    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        let parsed = parse_settings(
            "selected = /music/Deep Purple/=1\nfeed = https://example.com/f?format=rss\n",
        );
        assert_eq!(
            parsed.session.selected,
            Some(PathBuf::from("/music/Deep Purple/=1"))
        );
        assert_eq!(
            parsed.session.feed.as_deref(),
            Some("https://example.com/f?format=rss")
        );
    }

    /// Nothing set is written out, so the file stays readable and a missing key
    /// keeps meaning "no opinion" rather than "empty".
    #[test]
    fn an_empty_session_writes_no_session_keys() {
        let dir = std::env::temp_dir().join(format!("tuneterm-set-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("settings.txt");

        save_settings_to(&path, &Settings::default()).expect("save");
        let text = fs::read_to_string(&path).expect("read");
        for absent in [
            "tab", "folder", "selected", "feed", "track", "position", "playing",
        ] {
            assert!(
                !text.contains(&format!("{absent} =")),
                "{absent} was written"
            );
        }
        assert_eq!(load_settings_from(&path), Settings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The file is meant to be editable, so nothing in it can be trusted to be a
    /// number, let alone a sane one.
    #[test]
    fn a_hand_edited_settings_file_cannot_produce_a_mad_volume() {
        for (text, expected) in [
            ("volume = 5\n", MAX_VOLUME),
            ("volume = -3\n", 0.0),
            ("volume = NaN\n", 1.0),
            ("volume = inf\n", 1.0),
            ("volume = loud\n", 1.0),
            ("volume\n", 1.0),
            ("volume =\n", 1.0),
            ("# volume = 0.1\n", 1.0),
            ("  volume   =   0.25  \n", 0.25),
            ("nonsense = yes\nvolume = 0.5\n", 0.5),
        ] {
            assert_eq!(parse_settings(text).volume.get(), expected, "{text:?}");
        }
    }

    /// Same for the rest of it: a negative or unparsable position must not come
    /// back as a seek to somewhere impossible.
    #[test]
    fn a_hand_edited_position_falls_back_to_the_start() {
        for text in [
            "position = -5\n",
            "position = NaN\n",
            "position = soon\n",
            "position =\n",
        ] {
            assert_eq!(
                parse_settings(text).session.position,
                Duration::ZERO,
                "{text:?}"
            );
        }
        assert_eq!(
            parse_settings("position = 12.5\n").session.position,
            Duration::from_secs_f64(12.5)
        );
    }

    #[test]
    fn shuffle_accepts_the_ways_a_person_would_write_it() {
        for on in ["1", "true", "yes", "on"] {
            assert!(parse_settings(&format!("shuffle = {on}\n")).shuffle, "{on}");
        }
        for off in ["0", "false", "no", "off", "maybe"] {
            assert!(
                !parse_settings(&format!("shuffle = {off}\n")).shuffle,
                "{off}"
            );
        }
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
