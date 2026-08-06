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

**YouTube / YouTube Music** — shelling out to `yt-dlp` works and people do it. It is
fragile by nature (it breaks whenever the site changes) and sits outside YouTube's
terms. Reasonable as an opt-in, not as a default.

**Deezer, Tidal, Apple Music** — their public APIs are *metadata only*. Playback needs
reverse-engineered endpoints or a proprietary SDK. Not worth building on.

**Bandcamp** — no API. Purchased downloads are just files, so they already work via the
filesystem.

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

## Suggested order

1. **`Source` trait + async browse + HTTP seekable reader.** No new features, all the
   value. Local files become one implementation of the trait, which keeps it honest.
2. **Subsonic.** Highest payoff, and it validates the abstraction against a real API.
3. **WebDAV and podcasts.** Nearly free once the HTTP reader exists.
4. **Radio.** Forces the unknown-duration work, which everything else benefits from.
5. **Spotify**, behind a feature flag, if the Premium requirement is acceptable.

Steps 1 and 2 are the ones that matter. Everything after is incremental.
