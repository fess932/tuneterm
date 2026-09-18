//! The client side of `tuneterm serve`.
//!
//! The player is synchronous from top to bottom — workers and channels, no async —
//! and stays that way. The gRPC client runs on a small runtime of its own, and
//! everything here is a blocking call into it, meant for the worker threads that
//! already exist for exactly this kind of wait.
//!
//! # Addresses
//!
//! A server is written `tuneterm://[token@]host[:port]`, and a file on it
//! `tuneterm://host:port/path/inside`. Tracks carry the second form without the
//! token, so no secret ends up in the saved session; the token lives in the
//! [`Server`] registered for that host, or is the `token` from `settings.txt`.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status, Streaming};

use crate::library::{Folder, Track};
use crate::proto::library_service_client::LibraryServiceClient;
use crate::proto::{
    self, GetCoverRequest, ListFoldersRequest, ListTracksRequest, MAX_MESSAGE, ReadRequest,
    ReadResponse, StatRequest,
};

pub const SCHEME: &str = "tuneterm://";

/// How long a listing or a cover may take. A dead host should read as an error in
/// the status line, not as a player that stopped answering.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn is_remote(url: &str) -> bool {
    url.starts_with(SCHEME)
}

/// The runtime every blocking call here goes through. Two threads: the work is
/// waiting on a socket, not computing.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("grpc")
            .enable_all()
            .build()
            .expect("could not start the network runtime")
    })
}

/// Adds the token to every call.
#[derive(Clone)]
pub struct Auth(Option<MetadataValue<Ascii>>);

impl tonic::service::Interceptor for Auth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = &self.0 {
            request
                .metadata_mut()
                .insert("authorization", token.clone());
        }
        Ok(request)
    }
}

pub type Client = LibraryServiceClient<InterceptedService<Channel, Auth>>;

/// A server, parsed and ready to call. Cheap to clone: the connection is shared.
#[derive(Clone)]
pub struct Server {
    /// `host:port`, which is also how tracks name the server they came from.
    authority: String,
    client: Client,
}

/// Split `tuneterm://[token@]host[:port][/path]` into its parts.
fn parse(url: &str) -> Result<(Option<String>, String, String), String> {
    let rest = url
        .strip_prefix(SCHEME)
        .ok_or_else(|| format!("not a {SCHEME} address: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (token, host) = match authority.rsplit_once('@') {
        Some((token, host)) => (Some(token.to_string()), host),
        None => (None, authority),
    };
    if host.is_empty() {
        return Err(format!("no host in {url}"));
    }
    // A bare host gets the default port; `[::1]` style literals keep theirs.
    let has_port = match host.rfind(']') {
        Some(bracket) => host[bracket..].contains(':'),
        None => host.contains(':'),
    };
    let host = if has_port {
        host.to_string()
    } else {
        format!("{host}:{}", proto::DEFAULT_PORT)
    };
    Ok((token, host, path.to_string()))
}

/// The token for a server whose address carries none: `token` in `settings.txt`.
fn default_token() -> &'static Mutex<Option<String>> {
    static TOKEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    TOKEN.get_or_init(Default::default)
}

/// Set once at startup, from the settings, before any server is opened.
pub fn set_default_token(token: Option<String>) {
    *default_token().lock().expect("token lock poisoned") = token.filter(|t| !t.is_empty());
}

fn registry() -> &'static Mutex<HashMap<String, Server>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, Server>>> = OnceLock::new();
    SERVERS.get_or_init(Default::default)
}

impl Server {
    /// Parse `url` and prepare a connection, without making one: the first call
    /// connects. Also registers the server, so track URLs naming this host find
    /// it — and its token — later.
    pub fn open(url: &str) -> Result<Self, String> {
        let (token, authority, _) = parse(url)?;
        let token = token
            .or_else(|| default_token().lock().expect("token lock poisoned").clone())
            .filter(|token| !token.is_empty());
        let header = token
            .map(|token| {
                format!("Bearer {token}")
                    .parse::<MetadataValue<Ascii>>()
                    .map_err(|_| "the token has characters a header cannot carry".to_string())
            })
            .transpose()?;

        let endpoint = Endpoint::from_shared(format!("http://{authority}"))
            .map_err(|err| format!("{authority}: {err}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_nodelay(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_while_idle(true);
        // `connect_lazy` spawns the connection's background task, so it needs to
        // be inside the runtime.
        let channel = {
            let _inside = runtime().enter();
            endpoint.connect_lazy()
        };
        let client = LibraryServiceClient::with_interceptor(channel, Auth(header))
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);

        let server = Self { authority, client };
        registry()
            .lock()
            .expect("server registry poisoned")
            .insert(server.authority.clone(), server.clone());
        Ok(server)
    }

    /// The server a track URL points at, and the path on it. Uses the registered
    /// server when there is one, so its token comes along.
    pub fn for_url(url: &str) -> Result<(Self, String), String> {
        let (_, authority, path) = parse(url)?;
        let known = registry()
            .lock()
            .expect("server registry poisoned")
            .get(&authority)
            .cloned();
        let server = match known {
            Some(server) => server,
            None => Self::open(url)?,
        };
        Ok((server, path))
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The URL a track on this server goes by.
    pub fn url_for(&self, path: &str) -> String {
        format!(
            "{SCHEME}{}/{}",
            self.authority,
            path.trim_start_matches('/')
        )
    }

    /// Subfolders of `dir`, in the shape the local listing has.
    pub fn folders(&self, dir: &Path) -> Result<Vec<Folder>, String> {
        let mut client = self.client();
        let request = timed(ListFoldersRequest { path: wire(dir) });
        let list = runtime()
            .block_on(client.list_folders(request))
            .map_err(|status| describe(&status))?
            .into_inner();
        Ok(list
            .folders
            .into_iter()
            .map(|folder| Folder {
                label: folder.name,
                path: local(&folder.path),
                count: folder.count as usize,
            })
            .collect())
    }

    /// Every track at or below `dir`. `path` is the one the server knows it by, with
    /// a leading `/`, so it reads like a local path and stays a stable identity.
    pub fn tracks(&self, dir: &Path) -> Result<Vec<Track>, String> {
        let mut client = self.client();
        let request = timed(ListTracksRequest { path: wire(dir) });
        let list = runtime()
            .block_on(client.list_tracks(request))
            .map_err(|status| describe(&status))?
            .into_inner();
        Ok(list
            .tracks
            .into_iter()
            .map(|track| {
                let url = self.url_for(&track.path);
                Track {
                    path: local(&track.path),
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    duration: track.duration_ms.map(Duration::from_millis),
                    // The cover is asked for by the same URL: the server knows how to
                    // find a track's art, the way the local library does.
                    art_url: Some(url.clone()),
                    url: Some(url),
                }
            })
            .collect())
    }

    pub fn cover(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let mut client = self.client();
        let request = timed(GetCoverRequest { path: path.into() });
        let cover = runtime()
            .block_on(client.get_cover(request))
            .map_err(|status| describe(&status))?
            .into_inner();
        Ok((!cover.data.is_empty()).then_some(cover.data))
    }

    pub fn stat(&self, path: &str) -> Result<proto::StatResponse, String> {
        let mut client = self.client();
        let request = timed(StatRequest { path: path.into() });
        Ok(runtime()
            .block_on(client.stat(request))
            .map_err(|status| describe(&status))?
            .into_inner())
    }
}

/// Cover art for a track URL, for the cover worker.
pub fn cover(url: &str) -> Option<Vec<u8>> {
    let (server, path) = Server::for_url(url).ok()?;
    server.cover(&path).ok().flatten()
}

fn timed<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(CALL_TIMEOUT);
    request
}

/// A client-side path as the protocol writes it: relative, `/`-separated.
fn wire(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string()
}

/// A protocol path as the player holds it: rooted at `/`, like a local path.
fn local(path: &str) -> PathBuf {
    PathBuf::from(format!("/{}", path.trim_start_matches('/')))
}

/// A status as a person would want it in one line.
pub fn describe(status: &Status) -> String {
    match status.code() {
        tonic::Code::Unavailable => format!("server unreachable: {}", status.message()),
        tonic::Code::Unauthenticated => {
            "wrong or missing token: set `token = ...` in settings.txt".into()
        }
        tonic::Code::DeadlineExceeded => "the server took too long".into(),
        // A server built from an older schema answers a call it does not know with
        // no message at all.
        tonic::Code::Unimplemented => {
            "the server does not know this call; client and server versions differ".into()
        }
        _ if status.message().is_empty() => format!("{:?}", status.code()),
        _ => status.message().to_string(),
    }
}

/// A file on a server, readable and seekable like a local one.
///
/// The same shape as [`crate::net::HttpFile`]: reads run forwards through one open
/// stream, a seek drops it, and the next read opens another at the new offset.
pub struct RemoteFile {
    client: Client,
    path: String,
    len: u64,
    pos: u64,
    /// In a mutex only to be `Sync`, which rodio insists on and tonic's stream is
    /// not. Never contended: the decoder is the only reader.
    stream: Mutex<Option<Streaming<ReadResponse>>>,
    /// What is left of the last chunk.
    pending: Vec<u8>,
    taken: usize,
}

impl RemoteFile {
    pub fn open(url: &str) -> Result<Self, String> {
        let (server, path) = Server::for_url(url)?;
        let info = server.stat(&path)?;
        if !info.exists || info.is_dir {
            return Err(format!("not a file on the server: {path}"));
        }
        Ok(Self {
            client: server.client(),
            path,
            len: info.size,
            pos: 0,
            stream: Mutex::new(None),
            pending: Vec::new(),
            taken: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    fn drop_stream(&mut self) {
        *self.stream.get_mut().expect("stream lock poisoned") = None;
        self.pending.clear();
        self.taken = 0;
    }

    /// Refill `pending` from the stream, opening one if needed. False at the end.
    fn refill(&mut self) -> io::Result<bool> {
        let slot = self.stream.get_mut().expect("stream lock poisoned");
        if slot.is_none() {
            let mut client = self.client.clone();
            let request = ReadRequest {
                path: self.path.clone(),
                offset: self.pos,
            };
            let response = runtime()
                .block_on(client.read(request))
                .map_err(|status| io::Error::other(describe(&status)))?;
            *slot = Some(response.into_inner());
        }
        let stream = slot.as_mut().expect("just opened");
        match runtime().block_on(stream.message()) {
            Ok(Some(chunk)) => {
                self.pending = chunk.data;
                self.taken = 0;
                Ok(true)
            }
            Ok(None) => {
                *slot = None;
                Ok(false)
            }
            Err(status) => {
                *slot = None;
                Err(io::Error::other(describe(&status)))
            }
        }
    }
}

impl Read for RemoteFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        while self.taken >= self.pending.len() {
            if !self.refill()? {
                return Ok(0);
            }
        }
        let available = &self.pending[self.taken..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.taken += n;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RemoteFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.len as i64 + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        let target = (target as u64).min(self.len);
        if target == self.pos {
            return Ok(self.pos);
        }
        // Forwards within the chunk already here costs nothing; anywhere else
        // needs a new stream.
        let ahead = self.pending.len() - self.taken;
        if target > self.pos && target - self.pos <= ahead as u64 {
            self.taken += (target - self.pos) as usize;
        } else {
            self.drop_stream();
        }
        self.pos = target;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{TempDir, spawn_for_test, wav};

    /// Artist/Album/01.wav and 02.wav, plus a loose track at the top.
    fn library() -> TempDir {
        let dir = TempDir::new("remote");
        let album = dir.0.join("Artist").join("Альбом");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.wav"), wav(3)).unwrap();
        std::fs::write(album.join("02.wav"), wav(1)).unwrap();
        std::fs::write(dir.0.join("single.wav"), wav(1)).unwrap();
        dir
    }

    #[test]
    fn browses_like_the_local_library() {
        let lib = library();
        let addr = spawn_for_test(&lib.0, None);
        let server = Server::open(&format!("tuneterm://{addr}")).unwrap();

        let top = server.folders(Path::new("/")).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].label, "Artist");
        assert_eq!(top[0].path, PathBuf::from("/Artist"));
        assert_eq!(top[0].count, 2, "the count is recursive");

        let below = server.folders(Path::new("/Artist")).unwrap();
        assert_eq!(below[0].path, PathBuf::from("/Artist/Альбом"));

        // The whole discography from the artist, in one call.
        let tracks = server.tracks(Path::new("/Artist")).unwrap();
        let paths: Vec<_> = tracks.iter().map(|t| t.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/Artist/Альбом/01.wav"),
                PathBuf::from("/Artist/Альбом/02.wav")
            ]
        );
        assert_eq!(
            tracks[0].url.as_deref(),
            Some(format!("tuneterm://{addr}/Artist/Альбом/01.wav").as_str())
        );
        let duration = tracks[0].duration.expect("tags read on the server");
        assert!(duration.as_secs_f32() > 2.9, "{duration:?}");
    }

    #[test]
    fn a_remote_file_reads_and_seeks_like_a_local_one() {
        let lib = library();
        let addr = spawn_for_test(&lib.0, None);
        Server::open(&format!("tuneterm://{addr}")).unwrap();
        let want = std::fs::read(lib.0.join("Artist/Альбом/01.wav")).unwrap();

        let url = format!("tuneterm://{addr}/Artist/Альбом/01.wav");
        let mut file = RemoteFile::open(&url).unwrap();
        assert_eq!(file.len(), want.len() as u64);

        let mut all = Vec::new();
        file.read_to_end(&mut all).unwrap();
        assert!(all == want, "a straight read differs from the file");

        // Backwards, forwards past the buffered chunk, and within it.
        for at in [100u64, 40_000, 40_010, 7, want.len() as u64 - 5] {
            file.seek(SeekFrom::Start(at)).unwrap();
            let mut got = [0u8; 5];
            file.read_exact(&mut got).unwrap();
            assert_eq!(got, want[at as usize..at as usize + 5], "at {at}");
        }
        assert_eq!(file.read(&mut [0u8; 8]).unwrap(), 0, "past the end");
    }

    #[test]
    fn covers_come_back_as_bytes() {
        let lib = library();
        std::fs::write(lib.0.join("Artist/Альбом/cover.jpg"), b"not really a jpeg").unwrap();
        let addr = spawn_for_test(&lib.0, None);
        Server::open(&format!("tuneterm://{addr}")).unwrap();
        let with = cover(&format!("tuneterm://{addr}/Artist/Альбом/01.wav"));
        assert_eq!(with.as_deref(), Some(&b"not really a jpeg"[..]));
        assert_eq!(cover(&format!("tuneterm://{addr}/single.wav")), None);
    }

    #[test]
    fn nothing_outside_the_music_folder_is_reachable() {
        let lib = library();
        let outside = TempDir::new("outside");
        std::fs::write(outside.0.join("secret.wav"), wav(1)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside.0, lib.0.join("escape")).unwrap();

        let addr = spawn_for_test(&lib.0, None);
        let server = Server::open(&format!("tuneterm://{addr}")).unwrap();
        for path in ["../x", "Artist/../../x", "/../x"] {
            assert!(server.stat(path).is_err(), "{path} was let through");
        }
        #[cfg(unix)]
        {
            assert!(server.stat("escape/secret.wav").is_err());
            assert!(server.tracks(Path::new("/escape")).is_err());
        }
    }

    #[test]
    fn a_token_is_checked() {
        let lib = library();
        let addr = spawn_for_test(&lib.0, Some("right"));

        let wrong = Server::open(&format!("tuneterm://wrong@{addr}")).unwrap();
        let err = wrong
            .folders(Path::new("/"))
            .err()
            .expect("a wrong token got in");
        assert!(err.contains("token"), "{err}");

        let right = Server::open(&format!("tuneterm://right@{addr}")).unwrap();
        assert_eq!(right.folders(Path::new("/")).unwrap().len(), 1);
    }

    #[test]
    fn a_server_without_a_token_refuses_changes() {
        let lib = library();
        let addr = spawn_for_test(&lib.0, None);
        let server = Server::open(&format!("tuneterm://{addr}")).unwrap();
        let err = runtime()
            .block_on(server.client().remove(proto::RemoveRequest {
                path: "single.wav".into(),
                recursive: false,
            }))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(lib.0.join("single.wav").exists());
    }

    #[test]
    fn parses_addresses() {
        assert_eq!(
            parse("tuneterm://nas").unwrap(),
            (None, "nas:7700".into(), "/".into())
        );
        assert_eq!(
            parse("tuneterm://secret@nas:9000/Lumen/2002").unwrap(),
            (
                Some("secret".into()),
                "nas:9000".into(),
                "/Lumen/2002".into()
            )
        );
        assert_eq!(
            parse("tuneterm://[::1]/x").unwrap(),
            (None, "[::1]:7700".into(), "/x".into())
        );
        assert_eq!(
            parse("tuneterm://[::1]:1/x").unwrap().1,
            "[::1]:1".to_string()
        );
        assert!(parse("http://nas").is_err());
        assert!(parse("tuneterm:///x").is_err());
    }

    #[test]
    fn paths_round_trip() {
        assert_eq!(wire(Path::new("/")), "");
        assert_eq!(wire(Path::new("/Lumen/2002")), "Lumen/2002");
        assert_eq!(local("Lumen/2002"), PathBuf::from("/Lumen/2002"));
        assert_eq!(local(""), PathBuf::from("/"));
    }
}
