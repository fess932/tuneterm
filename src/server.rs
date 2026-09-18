//! `tuneterm serve`: one music folder, over gRPC.
//!
//! The server holds no index and no database. Every call walks the filesystem the
//! same way the player does locally — it is literally the same code in `library` —
//! so what a client browses is always what is on disk right now, including files
//! that were copied in by hand a second ago.
//!
//! # Paths
//!
//! Every path arrives relative to the music folder and is checked twice before it
//! touches the disk: lexically (no `..`, nothing absolute), and then against the
//! real filesystem, so a symlink inside the folder cannot be used to reach outside
//! it.
//!
//! # Changes
//!
//! Upload, remove and move need a token. A server started without one is read-only,
//! so running it on a trusted network with no setup at all is safe for the music.

use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::library;
use crate::proto::library_service_server::{LibraryService, LibraryServiceServer};
use crate::proto::{
    CHUNK, Entry, Folder, GetCoverRequest, GetCoverResponse, ListEntriesRequest,
    ListEntriesResponse, ListFoldersRequest, ListFoldersResponse, ListTracksRequest,
    ListTracksResponse, MAX_MESSAGE, MoveRequest, MoveResponse, ReadRequest, ReadResponse,
    RemoveRequest, RemoveResponse, StatRequest, StatResponse, Track, UploadRequest, UploadResponse,
};
use crate::worker::Cancel;

pub struct Config {
    pub root: PathBuf,
    pub listen: SocketAddr,
    /// `None` serves everyone and refuses every change.
    pub token: Option<String>,
}

/// Serve until interrupted. Builds its own runtime: nothing else in the process is
/// async, and the player never runs alongside the server.
pub fn run(config: Config) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("serve")
        .build()?;
    runtime.block_on(serve(config))
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let service = Service::new(&config.root, config.token.clone())?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let local = listener.local_addr()?;
    println!("serving {} on {local}", service.root.display());
    println!(
        "{}",
        match config.token {
            Some(_) => "token required; uploads, moves and removals allowed",
            None => "no TUNETERM_TOKEN: open to anyone and read-only",
        }
    );

    tonic::transport::Server::builder()
        .add_service(service.into_server(config.token))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            shutdown(),
        )
        .await?;
    println!("stopped");
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop`. Without the latter a container takes the
/// full ten-second grace period to die and is then killed.
async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => {
                    let _ = interrupt.await;
                    return;
                }
            };
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}

#[derive(Clone)]
pub struct Service {
    /// Canonical, so containment can be checked against canonical paths.
    root: Arc<PathBuf>,
    writable: bool,
}

impl Service {
    pub fn new(root: &Path, token: Option<String>) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", root.display()),
            ));
        }
        Ok(Self {
            root: Arc::new(root),
            writable: token.is_some(),
        })
    }

    /// The service with the token check in front of it.
    pub fn into_server(
        self,
        token: Option<String>,
    ) -> tonic::service::interceptor::InterceptedService<
        LibraryServiceServer<Service>,
        impl tonic::service::Interceptor + Clone,
    > {
        let expected = token.map(|token| format!("Bearer {token}"));
        let check = move |request: Request<()>| -> Result<Request<()>, Status> {
            let Some(expected) = &expected else {
                return Ok(request);
            };
            let given = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            match given {
                Some(given) if constant_time_eq(given.as_bytes(), expected.as_bytes()) => {
                    Ok(request)
                }
                _ => Err(Status::unauthenticated("wrong or missing token")),
            }
        };
        tonic::service::interceptor::InterceptedService::new(
            LibraryServiceServer::new(self)
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
            check,
        )
    }

    fn writable(&self) -> Result<(), Status> {
        if self.writable {
            Ok(())
        } else {
            Err(Status::permission_denied(
                "this server is read-only: start it with TUNETERM_TOKEN to allow changes",
            ))
        }
    }

    /// The real path for a request path, which must already exist and lie inside
    /// the root once symlinks are followed.
    fn existing(&self, rel: &str) -> Result<PathBuf, Status> {
        let path = self.root.join(lexical(rel)?);
        let real = path
            .canonicalize()
            .map_err(|_| Status::not_found(format!("no such path: /{}", rel.trim_matches('/'))))?;
        if !real.starts_with(&*self.root) {
            return Err(Status::permission_denied("outside the music folder"));
        }
        Ok(real)
    }

    /// The real path for something about to be created. Whatever part of it
    /// already exists must lie inside the root.
    fn creatable(&self, rel: &str) -> Result<PathBuf, Status> {
        let relative = lexical(rel)?;
        if relative.as_os_str().is_empty() {
            return Err(Status::invalid_argument("a path is required"));
        }
        let path = self.root.join(&relative);
        let mut ancestor = path.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().unwrap_or(&self.root);
        }
        let real = ancestor.canonicalize().map_err(internal)?;
        if !real.starts_with(&*self.root) {
            return Err(Status::permission_denied("outside the music folder"));
        }
        Ok(path)
    }

    /// The protocol's name for a real path: relative, `/`-separated.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&*self.root)
            .unwrap_or(path)
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Reject anything that could climb out of the root before the filesystem is even
/// asked. The empty path is the root itself.
fn lexical(rel: &str) -> Result<PathBuf, Status> {
    let mut out = PathBuf::new();
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => out.push(name),
            _ => return Err(Status::invalid_argument(format!("bad path: {rel}"))),
        }
    }
    Ok(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn internal(err: impl std::fmt::Display) -> Status {
    Status::internal(err.to_string())
}

/// Run filesystem work that blocks — tag reading above all — off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Status> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work).await.map_err(internal)?
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

#[tonic::async_trait]
impl LibraryService for Service {
    async fn list_folders(
        &self,
        request: Request<ListFoldersRequest>,
    ) -> Result<Response<ListFoldersResponse>, Status> {
        let dir = self.existing(&request.into_inner().path)?;
        let this = self.clone();
        let folders = blocking(move || {
            Ok(library::list_subdirs(&dir)
                .into_iter()
                .map(|folder| Folder {
                    path: this.relative(&folder.path),
                    name: folder.label,
                    count: folder.count as u32,
                })
                .collect())
        })
        .await?;
        Ok(Response::new(ListFoldersResponse { folders }))
    }

    async fn list_tracks(
        &self,
        request: Request<ListTracksRequest>,
    ) -> Result<Response<ListTracksResponse>, Status> {
        let dir = self.existing(&request.into_inner().path)?;
        let this = self.clone();
        let tracks = blocking(move || {
            Ok(library::scan_tracks_deep(&dir, &Cancel::never())
                .into_iter()
                .map(|track| Track {
                    size: std::fs::metadata(&track.path).map_or(0, |meta| meta.len()),
                    path: this.relative(&track.path),
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    duration_ms: track.duration.map(|d| d.as_millis() as u64),
                })
                .collect())
        })
        .await?;
        Ok(Response::new(ListTracksResponse { tracks }))
    }

    async fn get_cover(
        &self,
        request: Request<GetCoverRequest>,
    ) -> Result<Response<GetCoverResponse>, Status> {
        let path = self.existing(&request.into_inner().path)?;
        let data =
            blocking(move || Ok(library::load_cover_bytes(&path).unwrap_or_default())).await?;
        Ok(Response::new(GetCoverResponse { data }))
    }

    async fn stat(&self, request: Request<StatRequest>) -> Result<Response<StatResponse>, Status> {
        let info = match self.existing(&request.into_inner().path) {
            Ok(path) => {
                let meta = tokio::fs::metadata(&path).await.map_err(internal)?;
                StatResponse {
                    exists: true,
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                }
            }
            Err(status) if status.code() == tonic::Code::NotFound => StatResponse::default(),
            Err(status) => return Err(status),
        };
        Ok(Response::new(info))
    }

    type ReadStream = ChunkStream;

    async fn read(&self, request: Request<ReadRequest>) -> Result<Response<ChunkStream>, Status> {
        let request = request.into_inner();
        let path = self.existing(&request.path)?;
        let mut file = tokio::fs::File::open(&path).await.map_err(internal)?;
        file.seek(io::SeekFrom::Start(request.offset))
            .await
            .map_err(internal)?;

        // A small buffer: the stream only runs ahead of the reader by this many
        // chunks, so a client that seeks away wastes at most that much.
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                let mut data = vec![0u8; CHUNK];
                let n = match file.read(&mut data).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(err) => {
                        let _ = tx.send(Err(internal(err))).await;
                        return;
                    }
                };
                data.truncate(n);
                // The client hung up — usually a seek. Stop reading the disk.
                if tx.send(Ok(ReadResponse { data })).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn list_entries(
        &self,
        request: Request<ListEntriesRequest>,
    ) -> Result<Response<ListEntriesResponse>, Status> {
        let dir = self.existing(&request.into_inner().path)?;
        let mut read = tokio::fs::read_dir(&dir).await.map_err(internal)?;
        let mut entries = Vec::new();
        while let Some(entry) = read.next_entry().await.map_err(internal)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden, which includes uploads still in flight.
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            entries.push(Entry {
                name,
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
            });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(Response::new(ListEntriesResponse { entries }))
    }

    async fn upload(
        &self,
        request: Request<Streaming<UploadRequest>>,
    ) -> Result<Response<UploadResponse>, Status> {
        self.writable()?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty upload"))?;
        let header = first
            .header
            .ok_or_else(|| Status::invalid_argument("the first message needs a header"))?;
        let path = self.creatable(&header.path)?;
        if path.is_dir() {
            return Err(Status::already_exists(format!(
                "/{} is a folder",
                header.path
            )));
        }
        let parent = path.parent().unwrap_or(&self.root).to_path_buf();
        tokio::fs::create_dir_all(&parent).await.map_err(internal)?;

        // Hidden, so neither a listing nor a scan picks up half a file.
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let temp = parent.join(format!(".{name}.{}.part", std::process::id()));
        let result = receive(&temp, first.data, &mut stream, header.size).await;
        match result {
            Ok(()) => {
                tokio::fs::rename(&temp, &path).await.map_err(internal)?;
                println!("upload /{} ({} bytes)", self.relative(&path), header.size);
                Ok(Response::new(UploadResponse { size: header.size }))
            }
            Err(status) => {
                let _ = tokio::fs::remove_file(&temp).await;
                Err(status)
            }
        }
    }

    async fn remove(
        &self,
        request: Request<RemoveRequest>,
    ) -> Result<Response<RemoveResponse>, Status> {
        self.writable()?;
        let request = request.into_inner();
        let path = self.existing(&request.path)?;
        if path == *self.root {
            return Err(Status::permission_denied(
                "refusing to remove the music folder",
            ));
        }
        let meta = tokio::fs::symlink_metadata(&path).await.map_err(internal)?;
        let removed = if !meta.is_dir() {
            tokio::fs::remove_file(&path).await
        } else if request.recursive {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_dir(&path).await
        };
        removed.map_err(|err| match err.kind() {
            io::ErrorKind::DirectoryNotEmpty => {
                Status::failed_precondition("folder is not empty; remove it recursively")
            }
            _ => internal(err),
        })?;
        println!("remove /{}", self.relative(&path));
        Ok(Response::new(RemoveResponse {}))
    }

    async fn r#move(
        &self,
        request: Request<MoveRequest>,
    ) -> Result<Response<MoveResponse>, Status> {
        self.writable()?;
        let request = request.into_inner();
        let from = self.existing(&request.from)?;
        if from == *self.root {
            return Err(Status::permission_denied(
                "refusing to move the music folder",
            ));
        }
        let to = self.creatable(&request.to)?;
        if to.exists() {
            return Err(Status::already_exists(format!(
                "/{} already exists",
                request.to
            )));
        }
        if to.starts_with(&from) {
            return Err(Status::invalid_argument("cannot move a folder into itself"));
        }
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(internal)?;
        }
        tokio::fs::rename(&from, &to).await.map_err(internal)?;
        println!("move /{} -> /{}", self.relative(&from), self.relative(&to));
        Ok(Response::new(MoveResponse {}))
    }
}

/// Write an upload's bytes to `temp`, insisting on exactly `size` of them.
async fn receive(
    temp: &Path,
    first: Vec<u8>,
    stream: &mut Streaming<UploadRequest>,
    size: u64,
) -> Result<(), Status> {
    let mut file = tokio::fs::File::create(temp).await.map_err(internal)?;
    let mut written = 0u64;
    let mut data = first;
    loop {
        written += data.len() as u64;
        if written > size {
            return Err(Status::invalid_argument(
                "more data than the header announced",
            ));
        }
        file.write_all(&data).await.map_err(internal)?;
        match stream.message().await? {
            Some(chunk) => data = chunk.data,
            None => break,
        }
    }
    if written != size {
        return Err(Status::data_loss(format!(
            "upload ended after {written} of {size} bytes"
        )));
    }
    file.sync_all().await.map_err(internal)?;
    Ok(())
}

/// A server on a free local port, running on a runtime of its own for as long as
/// the test process lives. For tests on either side of the protocol.
#[cfg(test)]
pub fn spawn_for_test(root: &Path, token: Option<&str>) -> SocketAddr {
    let service = Service::new(root, token.map(str::to_string)).expect("test library");
    let token = token.map(str::to_string);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            tonic::transport::Server::builder()
                .add_service(service.into_server(token))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
    });
    rx.recv().expect("test server did not start")
}

/// A folder under the system temp directory, removed on drop.
#[cfg(test)]
pub struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tuneterm-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A playable WAV: `seconds` of a quiet ramp, mono 8 kHz 16-bit.
#[cfg(test)]
pub fn wav(seconds: u32) -> Vec<u8> {
    let rate = 8000u32;
    let samples = rate * seconds;
    let data_len = samples * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..samples {
        out.extend_from_slice(&((i % 256) as i16 * 16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_paths_stay_inside() {
        assert_eq!(lexical("").unwrap(), PathBuf::new());
        assert_eq!(lexical("/").unwrap(), PathBuf::new());
        assert_eq!(lexical("a/b").unwrap(), PathBuf::from("a").join("b"));
        assert_eq!(lexical("/a//b/").unwrap(), PathBuf::from("a").join("b"));
        assert_eq!(
            lexical("Лумен/2002").unwrap(),
            PathBuf::from("Лумен").join("2002")
        );
        for bad in ["..", "a/../..", "a/./b", "../etc/passwd"] {
            assert!(lexical(bad).is_err(), "{bad} was let through");
        }
    }

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"Bearer x", b"Bearer x"));
        assert!(!constant_time_eq(b"Bearer x", b"Bearer y"));
        assert!(!constant_time_eq(b"Bearer", b"Bearer x"));
    }
}
