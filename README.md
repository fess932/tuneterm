<div align="center">

# tuneterm

**A terminal music player that shows real album art.**

Three panes, mouse support, and covers rendered through the kitty / iTerm2 / sixel
graphics protocols — with a unicode-halfblock fallback so it degrades instead of
breaking.

[![CI](https://github.com/fess932/tuneterm/actions/workflows/ci.yml/badge.svg)](https://github.com/fess932/tuneterm/actions/workflows/ci.yml)
[![Release](https://github.com/fess932/tuneterm/actions/workflows/release.yml/badge.svg)](https://github.com/fess932/tuneterm/actions/workflows/release.yml)
![Rust](https://img.shields.io/badge/rust-2024%20edition-orange?logo=rust)
![License](https://img.shields.io/badge/license-MIT-blue)

</div>

```
╭ Library ───────────────╮╭ Tracks ───────────────────────╮╭ Now Playing ──────╮
│FOLDER              №   ││  #  TITLE          ARTIST TIME││                   │
│Deep Purple / =1    13  ││▶  1 Show Me        Deep P 3:41││   ▄▄▄▄▄▄▄▄▄▄▄▄▄   │
│Lumen / Диссонанс   20  ││   2 A Bit On The S Deep P 4:12││   █ real cover █  │
│Moby / Last Night   14  ││   3 Sharp Shooter  Deep P 3:29││   █   pixels  █   │
│                        ││   4 Portable Door  Deep P 3:12││   ▀▀▀▀▀▀▀▀▀▀▀▀▀   │
│                        ││                               ││                   │
│                        ││                               ││     Show Me       │
│                        ││                               ││    Deep Purple    │
│                        ││                               ││        =1         │
│                        ││                               ││ 1:02 / 3:41 ──────│
│                        ││                               ││╭───╮╭───────╮╭───╮│
│                        ││                               │││ ⏮ ││⏸ Pause││ ⏭ ││
│                        ││                               ││╰───╯╰───────╯╰───╯│
╰────────────────────────╯╰───────────────────────────────╯╰───────────────────╯
 Tab pane  ↑↓ move  ⏎ play  Space pause  n/p track  [/] seek  +/- vol  q quit
```

## Install

Grab a binary from [Releases](https://github.com/fess932/tuneterm/releases) —
macOS (Apple Silicon and Intel), Linux x86-64 and Windows x86-64 are built on
every tag.

```sh
tar xzf tuneterm-aarch64-apple-darwin.tar.gz
./tuneterm-aarch64-apple-darwin/tuneterm
```

Or build it yourself. No system libraries needed on macOS; on Linux you need ALSA
headers (`libasound2-dev`).

```sh
git clone https://github.com/fess932/tuneterm
cd tuneterm
cargo build --release      # a debug build is ~30x slower at drawing covers
./target/release/tuneterm
```

## Use

```sh
tuneterm                     # the Apple Music library if present, else ~/Music
tuneterm ~/path/to/music     # any folder tree
tuneterm --scan              # headless: dump folders, tags and cover sizes
tuneterm --no-media          # skip the OS media-key integration
tuneterm --help              # full help, including every key binding
```

There is a `Makefile` for the usual chores — `make` lists them:

```sh
make install       # into ~/.cargo/bin
make run MUSIC=~/Downloads
make check         # fmt + clippy + tests, exactly what CI runs
make bench         # print the benchmark numbers quoted below
make clean-cache   # empty the cover cache
```

`--scan` exists because a TUI swallows errors. Use it to confirm scanning, tag
reading and cover extraction work before blaming the rendering.

### Media keys

Play/pause, next and previous work from the keyboard's media keys, headphone
buttons, Control Center and MPRIS — **including while the terminal is in the
background**. Title, artist and cover art show up in the system's now-playing panel.

Media keys never reach a terminal app through stdin; the OS takes them first. So
there is no cheaper "only while focused" version — either the app registers with the
OS, which also makes it work in the background, or it does nothing at all.

| Platform | Mechanism | State |
| --- | --- | --- |
| macOS | `MPRemoteCommandCenter` | works; needs a run loop on the main thread |
| Linux | MPRIS over D-Bus | works; also gives `playerctl` and desktop widgets |
| Windows | `SystemMediaTransportControls` | builds; **runtime untested** |

Windows needs an `HWND`, which a console app has via `GetConsoleWindow()` — but under
a pseudo console (Windows Terminal, WezTerm) that window is the hidden ConPTY host,
and whether Windows accepts it for a media session is unverified. Failure is not
fatal: the player runs and says so in the status line.

`--no-media` skips the whole thing.

That macOS requirement is why `main` is shaped the way it is: AppKit only delivers to
the main thread's run loop, so the TUI runs on a worker thread and the main thread
services the OS. The app is built inside that worker because cpal's audio stream is
not `Send`.

### Keys

| Key | Action |
| --- | --- |
| `Tab`, `h`/`l`, `←`/`→` | switch pane |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn` | move selection |
| `Enter` | folders: jump to tracks · tracks: play |
| `Space` | play / pause |
| `n` / `p` | next / previous track |
| `[` / `]` | seek ∓5 s |
| `+` / `-` | volume |
| `q`, `Esc`, `Ctrl-C` | quit |

### Mouse

| Action | Effect |
| --- | --- |
| click a row | focus that pane and select the row |
| double-click a track | play it |
| click `⏮` / `⏭` | previous / next track |
| click `▶ Play` | toggle playback |
| click the progress bar | seek there |
| drag the progress bar | scrub |
| scroll wheel | moves **the pane under the cursor**, focused or not |

A single click never starts audio — that is what the second click is for.

Selecting a folder loads its tracks immediately, and playback advances to the next
track at the end of a file.

## How it works

| File | Role |
| --- | --- |
| `src/main.rs` | entry point, graphics-protocol detection, event loop, input |
| `src/app.rs` | all mutable state; no rendering |
| `src/ui.rs` | rendering only; writes back just the hit-test rects |
| `src/cover.rs` | cover-art worker thread: decode + scale, cached, with cancellation |
| `src/cache.rs` | on-disk cover cache, content-keyed, 200 MB cap, oldest-first eviction |
| `src/library.rs` | folder/track scanning, tags and cover extraction (`lofty`) |
| `src/player.rs` | thin `rodio` wrapper (play/pause/seek/position/volume) |
| `src/media.rs` | OS media keys and now-playing metadata (`souvlaki`) |

Built on [ratatui](https://ratatui.rs) +
[ratatui-image](https://github.com/benjajaja/ratatui-image) for drawing,
[rodio](https://github.com/RustAudio/rodio)/symphonia for audio and
[lofty](https://github.com/Serial-ATA/lofty-rs) for tags.

Cover art comes from the embedded picture tag first, then a sidecar file
(`cover.jpg`, `folder.jpg`, …) next to the track. Note that Apple Music does **not**
embed artwork in files — it keeps it in a separate cache — so its library shows no
covers.

## Notes from building it

Most of the interesting work was in things that are not obvious from the docs.

### Covers load on a worker thread

Album art is routinely 1400 px even though the pane is 300 px, and it *has* to be
shrunk to fit. In a debug build that costs real time:

| Cover in the file | decode | shrink + encode | total |
| --- | --- | --- | --- |
| 300 px | 13 ms | 14 ms | 27 ms |
| 1000 px | 96 ms | 240 ms | 337 ms |
| 1400 px | 190 ms | 399 ms | 589 ms |
| 3000 px | 865 ms | 1.4 s | 2.3 s |

Doing that inline made switching tracks feel like it hung. `src/cover.rs` moves
decoding *and* scaling onto one worker thread, so `play_index` now blocks for
0.6–20 ms — just long enough to start the audio — and the cover appears when it is
ready.

Requests are **replaced, not queued**: the slot holds at most one, so holding `n`
down cannot pile up work, and a job is dropped both before and after the expensive
part if a newer generation has been requested. One thread, ever.

Switching within an album shows the cover **in the same frame**, with no blank gap:
the decoded picture stays in memory, keyed by folder and pane size, and is reused
directly. That costs ~1 ms against the ~20 ms it takes to start the audio. The worker
still runs and replaces the image, so a folder with per-track art corrects itself
rather than being stuck. While a cover is in flight the event loop also polls at
15 ms instead of 120 ms, since the worker has no way to wake it.

### Scaled covers are cached on disk

Every track on an album carries the same picture, so the cache is keyed on the
**bytes of the source picture** (FNV-1a, ~0.2 ms) rather than the track path — one
entry per album, shared across runs. Scaling to a 600 px pane, debug build:

| Source | first track | rest of the album |
| --- | --- | --- |
| 300 px | 445 ms | **21 ms** |
| 1400 px | 1.6 s | **24 ms** |

Entries live in the platform cache directory (`~/Library/Caches/tuneterm`,
`$XDG_CACHE_HOME/tuneterm`, `%LOCALAPPDATA%\tuneterm`), capped at **200 MB** and
evicted oldest-first after each write. `TUNETERM_CACHE_DIR` overrides the location.

### Who scales the cover, and why it matters

The worker scales to *fill* the pane; the render thread never resizes. That split is
the whole point — doing it inline is what made switching tracks feel slow:

| drawn at | filter | first frame |
| --- | --- | --- |
| 300 px, as-is | — | 16 ms |
| 600 px (2×) | Nearest | 153 ms |
| 600 px (2×) | Lanczos3 | 450 ms |

Four times the pixels to resize, base64 and push through the terminal. The worker
pays that off-screen and uses Lanczos3, which it can afford.

`StatefulImage` letterboxes towards the top-left of the rect it is handed, so
centring is the caller's job — passing it the full pane width leaves a small cover
glued to the left edge. `art_cells()` is the single source of truth for the size and
`centre_in()` centres it; the layout reserves exactly those rows, so no rounding gap
can open between what is reserved and what is drawn.

Cells are not square, so all of this happens in pixels via `picker.font_size()` and
converts back to cells at the end.

### Russian tags that lie about their encoding

`Аффинаж — Дети` showed up as `Âàíÿ`, `Ñàøà`, `Ïàïà`. Not a decoder bug: the ID3v2.3
`TIT2` frame declares encoding `0x00` (ISO-8859-1) and then carries CP1251 bytes —
`c2 e0 ed ff` for `Ваня`. Decoding that per spec gives exactly the mojibake above,
which is why ffmpeg prints it too.

Very common in Russian MP3s, so tags are run through a recovery pass. It only fires
when the text really looks like that mistake: two or more high bytes which
*outnumber* the ASCII letters, and a CP1251 reading that comes out mostly Cyrillic.
Cyrillic-as-Latin-1 turns whole words into high bytes, while an accented Latin word
has one or two among ASCII letters — so `Björk`, `Motörhead`, `Sigur Rós` and
`Éléphant` are left alone. There are tests for each.

Tags carrying only an album artist and no track artist fall back to it.

### Graphics protocol detection avoids querying the terminal

| Terminal | Protocol used |
| --- | --- |
| kitty, Ghostty | Kitty — the only one with unicode placeholders |
| WezTerm, iTerm2, Konsole, mintty | iTerm2 inline images |
| foot, contour, mlterm, `*sixel*` | Sixel |
| anything else | unicode halfblocks |

The protocol comes from environment variables and the cell size from a `TIOCGWINSZ`
ioctl — no round-trip to the terminal. That is on purpose.

`Picker::from_query_stdio` (ratatui-image 11.0.6, `picker.rs::query_with_timeout`)
asks the terminal about its capabilities on a detached thread. If the terminal never
answers, the main thread gives up on its timeout **but that thread stays blocked in
`io::stdin().read()` forever** and from then on races crossterm for every keystroke.
The app then renders perfectly and ignores the keyboard completely. Reproducible in
any terminal that does not reply — proxied terminals, some multiplexers, CI.

`TUNETERM_QUERY=1` opts into the accurate detection when you know your terminal
answers.

WezTerm gets the iTerm2 path deliberately: its kitty implementation has no unicode
placeholders.

### Seeking needs the stream length

`player.rs` hands `Decoder::try_from` the `File` itself. Wrapping it in a
`BufReader` first hides the length, and symphonia then refuses to seek *backwards* —
forward works, so it is easy to miss. rodio also reports a seek as successful
without performing it when nothing is queued.

### chafa is not required

`ratatui-image`'s default features link the C library `chafa`, which only improves
the *halfblocks* fallback — kitty, sixel and iTerm2 do not touch it. It is disabled
in `Cargo.toml` so the build needs no system libraries. For nicer art on
graphics-less terminals:

```sh
brew install chafa
# then add "chafa-dyn" to ratatui-image's features
```

## Tests

```sh
cargo test                                    # 56 tests
make check                                    # what CI runs
cargo test -- --ignored --nocapture           # benchmarks, printed
```

The suite renders the whole UI into a `TestBackend` at sizes down to 8×3, checks the
cover geometry against a table of source sizes, drives clicks and drags through the
real hit-testing code, and plays actual audio — the fixture synthesises silent WAV
files so playback and seeking really run instead of falling into the error path.

## Where this could go

[PLAN.md](PLAN.md) sketches the other places music could come from — Subsonic,
WebDAV, podcasts, cloud storage, radio, Spotify — ranked by how well each fits the
table interface and what it would cost. The recurring conclusion is that the sources
are the easy part; the `Source` abstraction and a seekable HTTP reader are the work.

## Limitations

- No shuffle, no repeat, no playlist files (`.m3u`).
- Folder scanning is depth-limited to 5 and runs at startup, so a very large
  library pauses briefly before the first frame.
- rodio's decoders cover mp3, flac, m4a/aac, ogg/vorbis and wav. Opus does not work.
- Seeking a track that has already drained does nothing, per rodio.

## License

MIT — see [LICENSE](LICENSE).
