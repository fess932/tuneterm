# Other places to get music from

Notes on extending tuneterm past the local filesystem, and what it would cost.

The short version: **the sources are the easy part, the abstraction is the work.**
Once a `Source` trait exists and playback can read over the network, most of these
are a few hundred lines each. Without it, every one of them is a rewrite.

---

## What has to change first

Four things, in the order they bite.

### 1. `Track` stops being a path

`library.rs` hands out `Track { path: PathBuf, … }` and `player.rs` opens that path.
Everything downstream assumes a file exists. A remote track has an *id* — a Subsonic
song id, a URL, a Spotify URI — that only its own source knows how to turn into
bytes. So:

```rust
trait Source {
    fn browse(&self, at: &NodeId) -> Result<Vec<Node>>;   // folders / albums
    fn tracks(&self, at: &NodeId) -> Result<Vec<Track>>;
    fn open(&self, track: &TrackId) -> Result<Box<dyn Media>>;
    fn cover(&self, track: &TrackId) -> Result<Option<Vec<u8>>>;
}
```

`cover()` returning **bytes** rather than a path matters: `cache.rs` already keys on
the bytes of the picture, so remote art gets deduplication and the 200 MB rotation
for free, with no changes.

### 2. Playback needs a seekable reader

This is the real constraint, and it is not negotiable:

```rust
// rodio-0.22.2/src/decoder/builder.rs:135
impl<R: Read + Seek + Send + Sync + 'static> DecoderBuilder<R>
```

`Read + Seek`. Symphonia also refuses to seek backwards unless it knows the stream
length — that already bit us once, when a `BufReader` hid it. Over the network that
means one of:

- **HTTP range requests** — needs `Accept-Ranges: bytes` and a `Content-Length`.
  Works with Subsonic, WebDAV, S3, Drive. A `Read + Seek` adapter over ranged GETs
  with a read-ahead buffer is maybe 200 lines.
- **Download to a temp file first** — dead simple, correct seeking, but the track
  cannot start until enough has arrived. Fine as a first cut.
- **Neither** — live radio has no length and no ranges. See below.

The disk cache is already content-addressed with oldest-first eviction; audio can
reuse the same machinery under a separate budget.

### 3. Browsing has to go async

A network listing cannot block the render thread, and `scan_tracks` currently does
exactly that on every folder change. But the pattern is already built: `cover.rs` has
a worker with a **replaceable** request slot and generation-stamped replies, so a
fast-moving cursor cannot pile up work. Generalise that into one `Jobs` type and both
the cover loader and the browser use it.

### 4. Search stops being optional

Scrolling works for a folder tree. It does not work for a 100-million-track catalogue.
Any streaming source needs a search box before it is usable at all.

---

## The sources

Ranked by how well they fit the two-pane table, what they cost, and what can go wrong.

### Tier 1 — fits the existing model almost exactly

| Source | Why it fits | Effort |
| --- | --- | --- |
| **Subsonic / OpenSubsonic** | The best match by a distance | S |
| **Jellyfin / Emby** | Same shape, different REST | S |
| **WebDAV / Nextcloud** | It is just files over HTTP | S |
| **Podcasts (RSS)** | Feed = folder, episodes = tracks | XS |
| **SMB / NFS** | **Works today.** Mount it, point tuneterm at the mount | none |

**Subsonic is the one to do first.** Navidrome, Airsonic and Gonic all speak it, it is
a plain documented REST API, and it already models exactly what the interface shows:
browse by artist and album, a `stream` endpoint that serves real audio with a length
and range support, and `getCoverArt` for the picture. The `Source` trait above is
close to a transcription of it. Doing this one proves the abstraction.

**WebDAV** is PROPFIND to list and ranged GET to play — the same HTTP reader as
Subsonic, so it is nearly free once that exists.

**Podcasts** are the cheapest win of the lot: parse the feed, episodes have titles,
durations and artwork, and enclosures are plain HTTP files. It also needs *resume
position*, which is genuinely new state.

### Tier 2 — real work, no surprises

| Source | Catch |
| --- | --- |
| **S3 / MinIO** | Ranged GET works; needs signing |
| **Google Drive, Dropbox, OneDrive** | OAuth in a TUI: device-code, or a throwaway localhost callback |
| **DLNA / UPnP** | SSDP discovery plus SOAP. Clunky, but no accounts and no keys |

OAuth is the annoying part, not the file access. A device-code flow ("open this URL,
type this code") fits a terminal far better than a redirect.

### Tier 3 — possible, with caveats worth stating plainly

**Spotify** — [librespot](https://github.com/librespot-org/librespot) is Rust, is
maintained by librespot-org after the original author stepped back, and already backs
[ncspot](https://github.com/hrkfdn/ncspot), a Spotify TUI, so the shape is proven.
Two catches: it is **Premium-only by firm policy**, and it is an unofficial client, so
Spotify can break it and their terms are not written with it in mind. Technically it
also bypasses rodio entirely — librespot decodes and emits samples itself — so it is a
*second playback path*, not another `Source`. That is the largest single item here.

**YouTube Music** — see its own section below. Architecturally it is the *easiest*
of the streaming services, and the hard parts are elsewhere.

**Deezer, Tidal, Apple Music** — their public APIs are *metadata only*. Playback needs
reverse-engineered endpoints or a proprietary SDK. Not worth building on.

**Bandcamp** — no API. Purchased downloads are just files, so they already work via the
filesystem.

### YouTube Music, in more detail

Worth separating out, because it is the streaming service that fits this architecture
*best* — and for a reason that is easy to miss.

**It needs no second playback path.** Spotify has to go through librespot, which
decodes internally and bypasses rodio. YouTube hands back a plain HTTPS audio URL on
`googlevideo.com` that answers range requests and reports a length — which is exactly
what the `Read + Seek` reader from step 1 already needs. So once that reader exists,
playback is nearly free. Its catalogue is also the widest of anything here, and it
does not demand a paid tier the way librespot does.

The difficulty is entirely in *getting* the URL and in browsing.

| | Browsing | Stream URL | Licence | Runtime |
| --- | --- | --- | --- | --- |
| [rustypipe](https://codeberg.org/ThetaDev/rustypipe) | excellent | yes | **GPL-3.0** | Rust + tokio, plus a helper binary |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | weak | yes | Unlicense | Python |
| Innertube directly | do it yourself | yes | n/a | just HTTP |

**rustypipe** is a Rust Innertube client and covers YouTube Music properly: search,
albums, artists, playlists, radio, charts, saved items, history, even lyrics. It maps
onto the interface almost as neatly as Subsonic does. Two problems:

- **It is GPL-3.0.** Linking it into this MIT binary relicenses the whole thing.
  Either accept that, relicense, or keep it behind a process boundary.
- **PO tokens.** Since August 2024 YouTube requires a proof-of-origin token for
  streams, and rustypipe delegates that to a *separate* CLI, `rustypipe-botguard`.
  So it is not self-contained either.

**yt-dlp** is the pragmatic opposite: an external process, Unlicense in source form so
nothing propagates, and by far the best-maintained extractor because it is patched
whenever YouTube changes. `-f bestaudio -g` prints a URL; `--flat-playlist -J` prints
JSON. Its weakness is browsing — YouTube Music playlists are still not first-class
(yt-dlp#14591), so a music-shaped catalogue is awkward to walk.

**The likely split, then:** browse through Innertube or `rustypipe` in a separate
process, resolve stream URLs through `yt-dlp`, play through the ordinary HTTP reader.

**The real risk is not code, it is the arms race.** Signature deobfuscation, PO tokens
and bot detection change on YouTube's schedule, not ours, and both tools need frequent
updates to keep working. Extraction also sits outside YouTube's terms of service. That
argues for making it an opt-in feature that can fail loudly and be switched off,
rather than something the player depends on.

### Bonus: internet radio

[radio-browser.org](https://www.radio-browser.info/) is a free, documented, open
directory of stations, and streams are plain HTTP. Cheap to add and genuinely nice in
a terminal — but it is the one source that **breaks the current UI**, because a live
stream has no duration and no seeking. That makes it useful: it forces the interface
to handle the unknown-duration case honestly instead of pretending.

---

## What the UI has to grow

Independent of which source lands first:

- **A queue.** `n` / `p` currently mean "next row in this folder". With sources that
  have playlists and radios, playback order needs to be its own thing.
- **A third level.** Local music is folder → track. Streaming is artist → album →
  track. Either a breadcrumb in the left pane, or make it a tree.
- **Unknown duration.** The progress bar and the seek bar both assume `duration` is
  `Some`. Radio makes it `None` for real, not as an edge case.
- **A source picker.** Probably a bar above the left pane, or `:` commands.
- **Buffering and failure states.** A local file either opens or it does not. A network
  track can stall halfway, and the interface currently has no way to say so.

---

## Feeds and podcasts: the tabs that are nearly free

`musicforprogramming.net` prompted this section, and the useful discovery is that it
is **not a special case**. Its feed is an ordinary podcast RSS:

```
<enclosure url="https://datashat.net/music_for_programming_78-datassette.mp3"
           type="audio/mpeg" length="…"/>
<itunes:duration>…</itunes:duration>  <dc:creator>…</dc:creator>
<itunes:image href="…1024×1024…"/>
```

Every field the interface needs is already there, and the enclosure carries a
**length** — which is exactly the precondition for the `Read + Seek` reader, since
symphonia will not seek backwards without one. 78 episodes, plain MP3s, artwork that
the existing cover pipeline scales and content-hashes without changes.

So one RSS source covers musicforprogramming, every podcast, and any mix blog that
attaches enclosures. That is why `Feeds` and `Casts` are separate tabs but will share
almost all their code: the difference is presentation and *resume position*, which
podcasts need and a 90-minute mix does not.

### Where the list lives

A plain text file in the platform config directory, next to nothing else:

```
~/Library/Application Support/tuneterm/feeds.txt     # macOS
$XDG_CONFIG_HOME/tuneterm/feeds.txt                  # Linux
%APPDATA%\tuneterm\feeds.txt                        # Windows
```

One URL per line, `#` for comments, optional `name = url` when the feed's own title
is unhelpful. Shipped with `musicforprogramming.net/rss.xml` already in it.

Text, not TOML or JSON, for the same reason the cache directory is hand-rolled: it
needs no dependency, it is obvious in an editor, and a syntax error can only ever cost
one line rather than the whole file. Resume positions are *state*, not config, so they
belong in the cache directory instead — a second file keyed by episode URL.

### Adding one without breaking the design

The interface has no text input anywhere, which is the actual constraint. The answer
that fits is a **`:` command line** on the bottom row, where the help hints already
live — one row, appears only while typing, vanishes on Enter or Escape:

```
:add https://example.com/feed.xml
```

It earns its keep twice over, because [search](#4-search-stops-being-optional) needs
exactly the same affordance and would otherwise need inventing separately. `d` on a
selected feed removes it. No modal, no popup, nothing that has to be dismissed.

### Worth adding besides musicforprogramming

Ranked by whether there is something to build against, not by taste.

| Source | Interface | Notes |
| --- | --- | --- |
| [SomaFM](https://somafm.com/linktous/api.html) | `api.somafm.com/channels.json` | ~30 curated channels, documented for third-party clients. Their terms reserve logos, artwork and channel text, so use the API for streams and names |
| [Internet Archive](https://archive.org/advancedsearch.php) | JSON search + `metadata/<id>` | The widest legal catalogue here: Live Music Archive, netlabels, 78rpm digitisations. Ranged GET works |
| [radio-browser.org](https://www.radio-browser.info/) | free JSON API | Thousands of stations, no key |
| [Jamendo](https://developer.jamendo.com/) | REST, needs a key | Creative Commons catalogue with real streaming |
| [Musopen](https://musopen.org/) | small API | Public-domain classical |
| ccMixter, Free Music Archive | APIs of varying liveliness | CC catalogues; check the API still answers before relying on it |
| [ModArchive](https://modarchive.org/) | API | Tracker modules — but MOD/XM/IT need `libopenmpt`, so it is another playback path, not another source |

The ambient mix blogs in the same spirit as musicforprogramming — Ambient Blog,
Headphone Commute, Disquiet, A Strangely Isolated Place — mostly publish *article*
feeds, and only some attach enclosures. That is an argument for the generic feed
source rather than for hardcoding any of them: paste a URL, and it either has
enclosures and works or it does not and shows nothing.

### Cache

Episodes are 50–375 MB each, against ~200 KB for a cover, so the budgets are separate
and so are the directories:

| Kind | Directory | Budget |
| --- | --- | --- |
| Art | `<cache>/art` | 200 MB |
| Audio | `<cache>/audio` | 2000 MB |

Sharing one cap would let a single episode evict the entire art cache, and every cover
would then have to be decoded and scaled again. Both sweep oldest-first, and reads
touch the file, so what you actually listen to survives.

---

## Suggested order

1. **`Source` trait + async browse + HTTP seekable reader.** No new features, all the
   value. Local files become one implementation of the trait, which keeps it honest.
2. **Subsonic.** Highest payoff, and it validates the abstraction against a real API.
3. **Feeds and podcasts.** Nearly free once the HTTP reader exists, and
   musicforprogramming.net is the first entry. Brings the `:` command line, which
   search needs anyway.
4. **Radio.** Forces the unknown-duration work, which everything else benefits from.
5. **YouTube Music**, behind a feature flag. Cheap on the playback side once step 1
   exists; budget the time for browsing and for keeping extraction alive.
6. **Spotify**, only if Premium is acceptable and a second playback path is worth it.

Steps 1 and 2 are the ones that matter. Everything after is incremental.
