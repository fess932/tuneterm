//! Reading a podcast or mixtape RSS feed.
//!
//! Only the fields the interface shows, and no attempt at being a general feed
//! library: title, enclosure, duration, author, artwork. Anything absent simply
//! comes back empty rather than failing the whole feed, because one malformed item
//! should not cost the other seventy-seven.

use quick_xml::Reader;
use quick_xml::events::Event;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Episode {
    pub title: String,
    pub author: String,
    /// Where the audio is. An item without one is not playable and is dropped.
    pub url: String,
    pub duration: Option<Duration>,
    /// Per-item artwork, falling back to the channel's.
    pub art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Channel {
    pub title: String,
    pub episodes: Vec<Episode>,
}

/// Parse a feed. `Err` only for XML that cannot be read at all.
pub fn parse(xml: &str) -> Result<Channel, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut channel = Channel::default();
    let mut channel_art: Option<String> = None;

    // Which element's text we are currently inside, and the item being built.
    let mut path: Vec<String> = Vec::new();
    let mut item: Option<Episode> = None;

    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Eof => break,

            Event::Start(tag) => {
                let name = local_name(tag.name().as_ref());
                if name == "item" {
                    item = Some(Episode::default());
                }
                collect_attributes(&name, &tag, item.as_mut(), &mut channel_art);
                path.push(name);
            }

            // `<enclosure/>` and `<itunes:image/>` are usually self-closing, which is
            // a different event and easy to miss.
            Event::Empty(tag) => {
                let name = local_name(tag.name().as_ref());
                collect_attributes(&name, &tag, item.as_mut(), &mut channel_art);
            }

            Event::End(tag) => {
                let name = local_name(tag.name().as_ref());
                path.pop();
                if name == "item"
                    && let Some(episode) = item.take()
                    // No audio, nothing to play.
                    && !episode.url.is_empty()
                {
                    channel.episodes.push(episode);
                }
            }

            Event::Text(text) => {
                let Ok(value) = text.decode() else { continue };
                let value = value.trim().to_string();
                if value.is_empty() {
                    continue;
                }
                store_text(
                    path.last().map(String::as_str),
                    &value,
                    &mut item,
                    &mut channel,
                );
            }
            Event::CData(data) => {
                let value = String::from_utf8_lossy(data.as_ref()).trim().to_string();
                if value.is_empty() {
                    continue;
                }
                store_text(
                    path.last().map(String::as_str),
                    &value,
                    &mut item,
                    &mut channel,
                );
            }

            _ => {}
        }
    }

    // Items without their own artwork inherit the channel's.
    if let Some(art) = channel_art {
        for episode in &mut channel.episodes {
            if episode.art_url.is_none() {
                episode.art_url = Some(art.clone());
            }
        }
    }
    Ok(channel)
}

fn store_text(
    element: Option<&str>,
    value: &str,
    item: &mut Option<Episode>,
    channel: &mut Channel,
) {
    match (element, item.as_mut()) {
        (Some("title"), Some(episode)) => episode.title = value.to_string(),
        // `dc:creator` and `itunes:author` both appear in the wild.
        (Some("creator" | "author"), Some(episode)) if episode.author.is_empty() => {
            episode.author = value.to_string();
        }
        (Some("duration"), Some(episode)) => episode.duration = parse_duration(value),
        (Some("title"), None) if channel.title.is_empty() => channel.title = value.to_string(),
        _ => {}
    }
}

fn collect_attributes(
    name: &str,
    tag: &quick_xml::events::BytesStart,
    item: Option<&mut Episode>,
    channel_art: &mut Option<String>,
) {
    match name {
        "enclosure" => {
            if let Some(episode) = item
                && let Some(url) = attribute(tag, "url")
            {
                episode.url = url;
            }
        }
        "image" => {
            let href = attribute(tag, "href");
            match item {
                Some(episode) => episode.art_url = href,
                None if channel_art.is_none() => *channel_art = href,
                None => {}
            }
        }
        _ => {}
    }
}

fn attribute(tag: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    let found = tag.try_get_attribute(name).ok().flatten()?;
    // Feeds in the wild are XML 1.0; the parser needs telling which rules apply.
    let value = found
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Strip any `ns:` prefix. Namespaces are noise here — `itunes:duration` and a bare
/// `duration` mean the same thing to us.
fn local_name(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    match text.rsplit_once(':') {
        Some((_, local)) => local.to_ascii_lowercase(),
        None => text.to_ascii_lowercase(),
    }
}

/// `itunes:duration` is `SS`, `MM:SS` or `HH:MM:SS`, and sometimes plain seconds.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut seconds: u64 = 0;
    for part in &parts {
        // A fractional seconds field shows up occasionally.
        let value: u64 = part.trim().split('.').next()?.parse().ok()?;
        seconds = seconds * 60 + value;
    }
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the real musicforprogramming feed, which opens with a comment
    /// before `<rss>` and uses self-closing `enclosure` and `itunes:image`.
    const SAMPLE: &str = r#"<!-- Generated on Tue, 12 May 2026 16:11:03 GMT -->
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"
     xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Music For Programming</title>
    <itunes:image href="https://example.com/channel.jpg"/>
    <item>
      <title>78: Datassette</title>
      <dc:creator>Datassette</dc:creator>
      <itunes:duration>1:02:03</itunes:duration>
      <enclosure url="https://example.com/78.mp3" length="158925518" type="audio/mpeg"/>
      <itunes:image href="https://example.com/78.jpg"/>
    </item>
    <item>
      <title><![CDATA[77: Someone Else]]></title>
      <itunes:author>Someone Else</itunes:author>
      <itunes:duration>2520</itunes:duration>
      <enclosure url="https://example.com/77.mp3" type="audio/mpeg"/>
    </item>
    <item>
      <title>No audio here</title>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn reads_the_fields_the_interface_shows() {
        let channel = parse(SAMPLE).expect("parse");
        assert_eq!(channel.title, "Music For Programming");
        assert_eq!(
            channel.episodes.len(),
            2,
            "the item with no audio is dropped"
        );

        let first = &channel.episodes[0];
        assert_eq!(first.title, "78: Datassette");
        assert_eq!(first.author, "Datassette");
        assert_eq!(first.url, "https://example.com/78.mp3");
        assert_eq!(first.duration, Some(Duration::from_secs(3723)));
        assert_eq!(first.art_url.as_deref(), Some("https://example.com/78.jpg"));
    }

    #[test]
    fn cdata_titles_and_itunes_author_work() {
        let channel = parse(SAMPLE).unwrap();
        let second = &channel.episodes[1];
        assert_eq!(second.title, "77: Someone Else");
        assert_eq!(second.author, "Someone Else");
        assert_eq!(second.duration, Some(Duration::from_secs(2520)));
    }

    /// An item without its own artwork should still show something.
    #[test]
    fn channel_artwork_is_inherited() {
        let channel = parse(SAMPLE).unwrap();
        assert_eq!(
            channel.episodes[1].art_url.as_deref(),
            Some("https://example.com/channel.jpg")
        );
    }

    /// The channel title must not be overwritten by the first item's title.
    #[test]
    fn item_titles_do_not_leak_into_the_channel() {
        let channel = parse(SAMPLE).unwrap();
        assert_eq!(channel.title, "Music For Programming");
    }

    #[test]
    fn durations_in_every_shape() {
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("2:05"), Some(Duration::from_secs(125)));
        assert_eq!(parse_duration("1:00:00"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration(" 3:07 "), Some(Duration::from_secs(187)));
        assert_eq!(parse_duration("90.5"), Some(Duration::from_secs(90)));
        for bad in ["", "abc", "1:2:3:4", "0", "-5"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn namespaces_are_ignored() {
        assert_eq!(local_name(b"itunes:duration"), "duration");
        assert_eq!(local_name(b"DC:Creator"), "creator");
        assert_eq!(local_name(b"title"), "title");
    }

    /// A feed that is not a feed must come back empty, not panic.
    #[test]
    fn nonsense_input_is_survivable() {
        assert_eq!(parse("").unwrap().episodes.len(), 0);
        assert_eq!(
            parse("<html><body>hi</body></html>")
                .unwrap()
                .episodes
                .len(),
            0
        );
        assert!(parse("<rss><channel><item>").is_ok(), "truncated feed");
    }

    /// Against the real feed: `cargo test feed:: -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn parses_the_real_feed() {
        let bytes = crate::net::get("https://musicforprogramming.net/rss.xml").expect("fetch");
        let xml = String::from_utf8_lossy(&bytes);
        let channel = parse(&xml).expect("parse");
        println!("channel: {}", channel.title);
        println!("episodes: {}", channel.episodes.len());
        for episode in channel.episodes.iter().take(3) {
            println!(
                "  {} — {} [{:?}] {}",
                episode.author, episode.title, episode.duration, episode.url
            );
        }
        assert!(channel.episodes.len() > 50, "expected the full archive");
        assert!(
            channel.episodes.iter().all(|e| e.url.starts_with("http")),
            "every episode needs audio"
        );
    }
}
