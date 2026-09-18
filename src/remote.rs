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
//! [`Server`] registered for that host.

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

/// `host:port` of an address, for showing. `None` if it is not one.
pub fn authority_of(url: &str) -> Option<String> {
    parse(url).ok().map(|(_, authority, _)| authority)
}

/// The path part of an address, relative: `Lumen/2002` for
/// `tuneterm://nas/Lumen/2002`, and "" for the top of a server.
pub fn path_of(url: &str) -> String {
    parse(url)
        .map(|(_, _, path)| path.trim_matches('/').to_string())
        .unwrap_or_default()
}

/// Normalise what a person typed for a server — `nas`, `TOKEN@nas:7700`,
/// `tuneterm://TOKEN@nas` — into the address to keep and the token, separately.
pub fn split_address(typed: &str) -> Result<(String, Option<String>), String> {
    let typed = typed.trim();
    let url = if is_remote(typed) {
        typed.to_string()
    } else {
        format!("{SCHEME}{typed}")
    };
    let (token, authority, _) = parse(&url)?;
    Ok((
        format!("{SCHEME}{authority}"),
        token.filter(|token| !token.is_empty()),
    ))
}

fn registry() -> &'static Mutex<HashMap<String, Server>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, Server>>> = OnceLock::new();
    SERVERS.get_or_init(Default::default)
}

impl Server {
    /// Parse `url` and prepare a connection, without making one: the first call
    /// connects. The token is the one in the address, if any.
    pub fn open(url: &str) -> Result<Self, String> {
        Self::connect(url, None)
    }

    /// As [`Server::open`], with `token` for an address that carries none — the
    /// `token` from `settings.txt`.
    ///
    /// Registers the server, so track URLs naming this host find it — and its
    /// token — later, without ever carrying the token themselves.
    pub fn connect(url: &str, token: Option<&str>) -> Result<Self, String> {
        let (in_url, authority, _) = parse(url)?;
        let token = in_url
            .or_else(|| token.map(str::to_string))
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

    /// The server a URL points at, and the path on it, relative and without a
    /// leading `/`. Uses the registered server when there is one, so its token
    /// comes along.
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
        Ok((server, path.trim_matches('/').to_string()))
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The address of something on this server, `path` being relative to its
    /// music folder. The top of the server is `tuneterm://host:port`.
    pub fn url_for(&self, path: &str) -> String {
        match path.trim_matches('/') {
            "" => format!("{SCHEME}{}", self.authority),
            path => format!("{SCHEME}{}/{path}", self.authority),
        }
    }

    /// Subfolders of `path`, in the shape the local listing has. Each one's path is
    /// its address, so the browser can walk in and out of it like any folder.
    pub fn folders(&self, path: &str) -> Result<Vec<Folder>, String> {
        let mut client = self.client();
        let request = timed(ListFoldersRequest { path: path.into() });
        let list = runtime()
            .block_on(client.list_folders(request))
            .map_err(|status| describe(&status))?
            .into_inner();
        Ok(list
            .folders
            .into_iter()
            .map(|folder| Folder {
                label: folder.name,
                path: PathBuf::from(self.url_for(&folder.path)),
                count: folder.count as usize,
            })
            .collect())
    }

    /// Every track at or below `path`. A track's path is its address, the way a
    /// feed episode's is its URL.
    pub fn tracks(&self, path: &str) -> Result<Vec<Track>, String> {
        let mut client = self.client();
        let request = timed(ListTracksRequest { path: path.into() });
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
                    path: PathBuf::from(&url),
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

/// Subfolders of the folder at a server address.
pub fn folders(url: &str) -> Result<Vec<Folder>, String> {
    let (server, path) = Server::for_url(url)?;
    server.folders(&path)
}

/// Every track at or below the folder at a server address.
pub fn tracks(url: &str) -> Result<Vec<Track>, String> {
    let (server, path) = Server::for_url(url)?;
    server.tracks(&path)
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

/// One file of a [`push`], once the server has it.
#[derive(Debug, Clone)]
pub struct Sent {
    pub local: PathBuf,
    /// Where it is on the server, relative.
    pub remote: String,
    pub size: u64,
    /// Already there with the same size, so nothing was sent.
    pub skipped: bool,
}

/// What a [`push`] managed. `done` holds only files the server has confirmed in
/// full, which is what makes it safe to delete them afterwards even when `error`
/// says the push stopped partway.
#[derive(Debug, Default)]
pub struct Pushed {
    pub done: Vec<Sent>,
    pub total: usize,
    pub error: Option<String>,
}

/// Upload a file, or a folder with everything beneath it, to `base` on the server.
/// Hidden files stay behind. Files already there with the same size are skipped.
///
/// Blocking. `each` hears about every file as it completes, for progress.
pub fn push(
    server: &Server,
    local: &Path,
    base: &str,
    mut each: impl FnMut(usize, usize, &Sent),
) -> Pushed {
    let base = base.trim_matches('/');
    let files = match std::fs::metadata(local) {
        Ok(meta) if meta.is_dir() => {
            let mut files = Vec::new();
            if let Err(err) = walk(local, local, &mut files) {
                return Pushed {
                    error: Some(format!("{}: {err}", local.display())),
                    ..Pushed::default()
                };
            }
            files
                .into_iter()
                .map(|(path, rel)| (path, join(base, &rel)))
                .collect()
        }
        Ok(_) => vec![(local.to_path_buf(), base.to_string())],
        Err(err) => {
            return Pushed {
                error: Some(format!("{}: {err}", local.display())),
                ..Pushed::default()
            };
        }
    };

    let mut pushed = Pushed {
        total: files.len(),
        ..Pushed::default()
    };
    for (index, (path, remote)) in files.into_iter().enumerate() {
        match push_one(server, &path, &remote) {
            Ok(sent) => {
                each(index + 1, pushed.total, &sent);
                pushed.done.push(sent);
            }
            Err(err) => {
                pushed.error = Some(format!("/{remote}: {err}"));
                break;
            }
        }
    }
    pushed
}

fn push_one(server: &Server, path: &Path, remote: &str) -> Result<Sent, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let there = server.stat(remote)?;
    let sent = Sent {
        local: path.to_path_buf(),
        remote: remote.to_string(),
        size,
        skipped: there.exists && !there.is_dir && there.size == size,
    };
    if !sent.skipped {
        let confirmed = runtime().block_on(upload(server, path, remote, size))?;
        if confirmed != size {
            return Err(format!("the server has {confirmed} of {size} bytes"));
        }
    }
    Ok(sent)
}

/// Stream one file up. Returns the size the server says it stored.
async fn upload(server: &Server, path: &Path, remote: &str, size: u64) -> Result<u64, String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| e.to_string())?;
    let header = proto::UploadHeader {
        path: remote.to_string(),
        size,
    };
    // The header rides on the first chunk; the rest follow as the disk yields
    // them, so a big file never sits in memory whole.
    let (tx, rx) = tokio::sync::mpsc::channel::<proto::UploadRequest>(4);
    let reader = tokio::spawn(async move {
        let mut header = Some(header);
        loop {
            let mut data = vec![0u8; proto::CHUNK];
            let n = file.read(&mut data).await?;
            data.truncate(n);
            let last = n == 0;
            if !last || header.is_some() {
                let chunk = proto::UploadRequest {
                    header: header.take(),
                    data,
                };
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
            if last {
                break;
            }
        }
        std::io::Result::Ok(())
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = server.client().upload(stream).await;
    reader
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(result
        .map_err(|status| describe(&status))?
        .into_inner()
        .size)
}

/// Every file under `dir`, hidden ones left out, with its path relative to `base`
/// written the way the protocol wants it.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            walk(base, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(io::Error::other)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel));
        }
    }
    Ok(())
}

/// `dir/name` in the protocol's form, where "" is the top.
pub fn join(dir: &str, name: &str) -> String {
    match (dir.trim_matches('/'), name.trim_matches('/')) {
        ("", name) => name.to_string(),
        (dir, "") => dir.to_string(),
        (dir, name) => format!("{dir}/{name}"),
    }
}

/// Delete what a push confirmed, then any folder under `local` it left empty.
/// Files the push did not confirm, and hidden files it never sent, stay — and so
/// do the folders holding them.
pub fn remove_moved(local: &Path, pushed: &Pushed) -> io::Result<usize> {
    let mut removed = 0;
    for sent in &pushed.done {
        std::fs::remove_file(&sent.local)?;
        removed += 1;
    }
    if local.is_dir() {
        prune(local)?;
    }
    Ok(removed)
}

/// Remove empty folders bottom-up, `dir` included.
fn prune(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            prune(&path)?;
        }
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        // Not empty: something that was not moved is still in it.
        Err(_)
            if std::fs::read_dir(dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// A status as a person would want it in one line.
pub fn describe(status: &Status) -> String {
    match status.code() {
        tonic::Code::Unavailable => format!("server unreachable: {}", status.message()),
        tonic::Code::Unauthenticated => "wrong or missing token for the server".into(),
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

        // A folder's path is its address, so the browser can walk into it.
        let top = folders(&format!("tuneterm://{addr}")).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].label, "Artist");
        assert_eq!(top[0].path, PathBuf::from(format!("tuneterm://{addr}/Artist")));
        assert_eq!(top[0].count, 2, "the count is recursive");

        let below = folders(&top[0].path.to_string_lossy()).unwrap();
        assert_eq!(
            below[0].path,
            PathBuf::from(format!("tuneterm://{addr}/Artist/Альбом"))
        );

        // The whole discography from the artist, in one call.
        let tracks = server.tracks("Artist").unwrap();
        let paths: Vec<_> = tracks.iter().map(|t| t.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from(format!("tuneterm://{addr}/Artist/Альбом/01.wav")),
                PathBuf::from(format!("tuneterm://{addr}/Artist/Альбом/02.wav"))
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
            assert!(server.tracks("escape").is_err());
        }
    }

    #[test]
    fn a_token_is_checked() {
        let lib = library();
        let addr = spawn_for_test(&lib.0, Some("right"));

        let wrong = Server::open(&format!("tuneterm://wrong@{addr}")).unwrap();
        let err = wrong.folders("").err().expect("a wrong token got in");
        assert!(err.contains("token"), "{err}");

        let right = Server::open(&format!("tuneterm://right@{addr}")).unwrap();
        assert_eq!(right.folders("").unwrap().len(), 1);

        // A token kept apart from the address, the way the settings keep it.
        let kept = Server::connect(&format!("tuneterm://{addr}"), Some("right")).unwrap();
        assert_eq!(kept.folders("").unwrap().len(), 1);
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
    fn addresses_split_into_parts() {
        assert_eq!(path_of("tuneterm://nas/Lumen/2002"), "Lumen/2002");
        assert_eq!(path_of("tuneterm://nas"), "");
        assert_eq!(authority_of("tuneterm://k@nas/x").as_deref(), Some("nas:7700"));
        for (typed, server, token) in [
            ("nas", "tuneterm://nas:7700", None),
            ("  k@nas:9 ", "tuneterm://nas:9", Some("k")),
            ("tuneterm://k@nas", "tuneterm://nas:7700", Some("k")),
        ] {
            let (got_server, got_token) = split_address(typed).unwrap();
            assert_eq!(got_server, server, "{typed}");
            assert_eq!(got_token.as_deref(), token, "{typed}");
        }
        assert!(split_address("").is_err());
    }

    #[test]
    fn a_move_deletes_only_what_the_server_confirmed() {
        let local = TempDir::new("move-src");
        let album = local.0.join("Lumen").join("2002");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.wav"), wav(1)).unwrap();
        std::fs::write(album.join("02.wav"), wav(1)).unwrap();
        std::fs::write(local.0.join("Lumen").join(".DS_Store"), b"x").unwrap();
        let served = TempDir::new("move-dst");
        let addr = spawn_for_test(&served.0, Some("k"));
        let server = Server::open(&format!("tuneterm://k@{addr}")).unwrap();

        // Only one of the two confirmed: the other must stay.
        let mut pushed = push(&server, &local.0.join("Lumen"), "Lumen", |_, _, _| {});
        assert!(pushed.error.is_none(), "{:?}", pushed.error);
        assert_eq!(pushed.done.len(), 2, "the hidden file is not sent");
        pushed.done.truncate(1);
        remove_moved(&local.0.join("Lumen"), &pushed).unwrap();
        assert!(!album.join("01.wav").exists());
        assert!(album.join("02.wav").exists(), "an unconfirmed file was deleted");
        assert!(served.0.join("Lumen/2002/02.wav").is_file());
    }
}
