//! The commands that are not the player: running a server, and managing the music
//! on one from the terminal.
//!
//! ```text
//! tuneterm serve ~/Music
//! tuneterm push ~/Downloads/Lumen            # -> /Lumen on the server
//! tuneterm ls /Lumen
//! tuneterm mv /Lumen/old /Lumen/new
//! tuneterm rm -r /Lumen/junk
//! ```
//!
//! The server comes from `--server` or `TUNETERM_SERVER`, the token from the
//! address or `TUNETERM_TOKEN`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::io::AsyncReadExt;

use crate::proto::{
    self, CHUNK, ListEntriesRequest, MoveRequest, RemoveRequest, StatRequest, UploadHeader,
    UploadRequest,
};
use crate::remote::{self, Server};

/// The first argument, when it names one of these, decides the whole run.
pub const COMMANDS: &[&str] = &["serve", "push", "ls", "rm", "mv"];

pub fn help() -> String {
    format!(
        "\
SERVER
    {name} serve [FOLDER] [--listen ADDR]
                      Serve FOLDER (or $TUNETERM_MUSIC) over gRPC on ADDR,
                      0.0.0.0:{port} by default. With TUNETERM_TOKEN set, clients
                      must present it and may change the library; without it
                      the server is open and read-only.

    {name} tuneterm://[TOKEN@]HOST[:PORT]
                      Browse a server in the player exactly like a local folder.

    {name} push LOCAL [REMOTE]
                      Upload a file or a folder with everything beneath it.
                      A folder lands as /NAME unless REMOTE says otherwise.
                      Files already there with the same size are skipped.
    {name} ls [PATH]
                      List a folder on the server.
    {name} mv FROM TO
                      Move or rename on the server.
    {name} rm [-r] PATH
                      Remove a file, or a folder with -r.

    These take --server tuneterm://HOST[:PORT], or TUNETERM_SERVER.",
        name = env!("CARGO_PKG_NAME"),
        port = proto::DEFAULT_PORT,
    )
}

/// Run a command. `args` starts with the command's name.
pub fn run(args: Vec<String>) -> anyhow::Result<()> {
    let (command, rest) = args.split_first().expect("dispatched on a command name");
    let mut options = Options::parse(rest)?;
    match command.as_str() {
        "serve" => serve(options),
        other => {
            let server = options.server()?;
            let runtime = remote::runtime();
            match other {
                "push" => runtime.block_on(push(&server, &options.positional)),
                "ls" => runtime.block_on(ls(&server, options.positional.first())),
                "mv" => runtime.block_on(mv(&server, &options.positional)),
                "rm" => runtime.block_on(rm(&server, &options.positional, options.recursive)),
                _ => unreachable!("not in COMMANDS"),
            }
        }
    }
}

#[derive(Default)]
struct Options {
    positional: Vec<String>,
    server: Option<String>,
    listen: Option<String>,
    recursive: bool,
}

impl Options {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => options.server = Some(value(&mut args, arg)?),
                "--listen" => options.listen = Some(value(&mut args, arg)?),
                "-r" | "--recursive" => options.recursive = true,
                other if other.starts_with('-') && other.len() > 1 => {
                    anyhow::bail!("unknown option: {other}\n\nTry --help.")
                }
                other => options.positional.push(other.to_string()),
            }
        }
        Ok(options)
    }

    fn server(&mut self) -> anyhow::Result<Server> {
        let address = self
            .server
            .take()
            .or_else(|| std::env::var("TUNETERM_SERVER").ok())
            .filter(|address| !address.is_empty())
            .ok_or_else(|| anyhow::anyhow!("which server? pass --server or set TUNETERM_SERVER"))?;
        let address = if remote::is_remote(&address) {
            address
        } else {
            format!("{}{address}", remote::SCHEME)
        };
        Server::open(&address).map_err(anyhow::Error::msg)
    }
}

fn value<'a>(args: &mut impl Iterator<Item = &'a String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn serve(options: Options) -> anyhow::Result<()> {
    let root = options
        .positional
        .first()
        .cloned()
        .or_else(|| std::env::var("TUNETERM_MUSIC").ok())
        .ok_or_else(|| anyhow::anyhow!("which folder? tuneterm serve FOLDER"))?;
    let listen = options
        .listen
        .or_else(|| std::env::var("TUNETERM_LISTEN").ok())
        .unwrap_or_else(|| format!("0.0.0.0:{}", proto::DEFAULT_PORT));
    let listen: SocketAddr = listen
        .parse()
        .map_err(|_| anyhow::anyhow!("--listen wants ADDRESS:PORT, got {listen}"))?;
    let token = std::env::var("TUNETERM_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    crate::server::run(crate::server::Config {
        root: PathBuf::from(root),
        listen,
        token,
    })
}

fn status(err: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!(remote::describe(&err))
}

/// A remote path as typed, normalised to the protocol's form.
fn wire(path: &str) -> String {
    path.trim_matches('/').to_string()
}

fn join(dir: &str, name: &str) -> String {
    match wire(dir).as_str() {
        "" => name.to_string(),
        dir => format!("{dir}/{name}"),
    }
}

async fn ls(server: &Server, path: Option<&String>) -> anyhow::Result<()> {
    let path = path.map(|path| wire(path)).unwrap_or_default();
    let entries = server
        .client()
        .list_entries(ListEntriesRequest { path })
        .await
        .map_err(status)?
        .into_inner()
        .entries;
    for entry in entries {
        if entry.is_dir {
            println!("{:>10}  {}/", "", entry.name);
        } else {
            println!("{:>10}  {}", human(entry.size), entry.name);
        }
    }
    Ok(())
}

async fn mv(server: &Server, args: &[String]) -> anyhow::Result<()> {
    let [from, to] = args else {
        anyhow::bail!("usage: tuneterm mv FROM TO");
    };
    server
        .client()
        .r#move(MoveRequest {
            from: wire(from),
            to: wire(to),
        })
        .await
        .map_err(status)?;
    println!("moved /{} -> /{}", wire(from), wire(to));
    Ok(())
}

async fn rm(server: &Server, args: &[String], recursive: bool) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: tuneterm rm [-r] PATH...");
    }
    for path in args {
        let path = wire(path);
        // The whole library in one keystroke is not something to allow by accident.
        if path.is_empty() {
            anyhow::bail!("refusing to remove the root of the library");
        }
        server
            .client()
            .remove(RemoveRequest {
                path: path.clone(),
                recursive,
            })
            .await
            .map_err(status)?;
        println!("removed /{path}");
    }
    Ok(())
}

async fn push(server: &Server, args: &[String]) -> anyhow::Result<()> {
    let (local, target) = match args {
        [local] => (PathBuf::from(local), None),
        [local, target] => (PathBuf::from(local), Some(wire(target))),
        _ => anyhow::bail!("usage: tuneterm push LOCAL [REMOTE]"),
    };
    let meta =
        std::fs::metadata(&local).map_err(|err| anyhow::anyhow!("{}: {err}", local.display()))?;
    let name = local
        .canonicalize()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("{} has no name to upload under", local.display()))?;

    // Like cp: a target that is an existing folder receives the upload inside it,
    // any other target names the destination itself, and without one the file or
    // folder keeps its own name at the top of the library.
    let base = match target.filter(|t| !t.is_empty()) {
        None => name,
        Some(target) => {
            let there = server
                .client()
                .stat(StatRequest {
                    path: target.clone(),
                })
                .await
                .map_err(status)?
                .into_inner();
            if there.is_dir {
                join(&target, &name)
            } else {
                target
            }
        }
    };
    let files = if meta.is_dir() {
        let mut files = Vec::new();
        walk(&local, &local, &mut files)?;
        files
            .into_iter()
            .map(|(path, rel)| (path, join(&base, &rel)))
            .collect()
    } else {
        vec![(local.clone(), base)]
    };

    let started = Instant::now();
    let (mut sent, mut skipped, mut bytes) = (0usize, 0usize, 0u64);
    let total = files.len();
    for (index, (path, remote)) in files.into_iter().enumerate() {
        let size = std::fs::metadata(&path)?.len();
        let there = server
            .client()
            .stat(StatRequest {
                path: remote.clone(),
            })
            .await
            .map_err(status)?
            .into_inner();
        let counter = format!("[{}/{total}]", index + 1);
        if there.exists && !there.is_dir && there.size == size {
            skipped += 1;
            println!("{counter} same   /{remote}");
            continue;
        }
        upload(server, &path, &remote, size).await?;
        sent += 1;
        bytes += size;
        println!("{counter} sent   /{remote}  {}", human(size));
    }

    if sent == 0 {
        println!("nothing to send, {skipped} already there");
    } else {
        let seconds = started.elapsed().as_secs_f64().max(0.001);
        println!(
            "{sent} sent ({}, {}/s), {skipped} already there",
            human(bytes),
            human((bytes as f64 / seconds) as u64)
        );
    }
    Ok(())
}

/// Every file under `dir`, hidden ones left out, with its path relative to `base`
/// written the way the protocol wants it.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) -> anyhow::Result<()> {
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
                .strip_prefix(base)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel));
        }
    }
    Ok(())
}

async fn upload(server: &Server, path: &Path, remote: &str, size: u64) -> anyhow::Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let header = UploadHeader {
        path: remote.to_string(),
        size,
    };
    // The header rides on the first chunk; the rest follow as the disk yields
    // them, so a big file never sits in memory whole.
    let (tx, rx) = tokio::sync::mpsc::channel::<UploadRequest>(4);
    let reader = tokio::spawn(async move {
        let mut header = Some(header);
        loop {
            let mut data = vec![0u8; CHUNK];
            let n = file.read(&mut data).await?;
            data.truncate(n);
            let last = n == 0;
            if !last || header.is_some() {
                let chunk = UploadRequest {
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
        anyhow::Ok(())
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = server.client().upload(stream).await;
    reader.await??;
    result.map_err(status)?;
    Ok(())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{TempDir, spawn_for_test, wav};

    fn tree(root: &Path) {
        let album = root.join("Lumen").join("2002 - Правда");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.wav"), wav(1)).unwrap();
        std::fs::write(album.join("02.wav"), wav(2)).unwrap();
        std::fs::write(album.join("cover.jpg"), b"jpeg").unwrap();
        std::fs::write(root.join("Lumen").join(".DS_Store"), b"junk").unwrap();
        // Bigger than one chunk, so an upload is really a stream.
        std::fs::write(root.join("Lumen").join("long.wav"), wav(40)).unwrap();
    }

    #[test]
    fn push_uploads_a_tree_and_skips_what_is_there() {
        let local = TempDir::new("push-src");
        let served = TempDir::new("push-dst");
        tree(&local.0);
        let addr = spawn_for_test(&served.0, Some("k"));
        let server = Server::open(&format!("tuneterm://k@{addr}")).unwrap();
        let src = local.0.join("Lumen").to_string_lossy().into_owned();

        remote::runtime()
            .block_on(push(&server, std::slice::from_ref(&src)))
            .unwrap();
        for rel in [
            "2002 - Правда/01.wav",
            "2002 - Правда/02.wav",
            "2002 - Правда/cover.jpg",
            "long.wav",
        ] {
            assert_eq!(
                std::fs::read(served.0.join("Lumen").join(rel)).unwrap(),
                std::fs::read(local.0.join("Lumen").join(rel)).unwrap(),
                "{rel}"
            );
        }
        assert!(
            !served.0.join("Lumen/.DS_Store").exists(),
            "hidden files stay home"
        );
        let leftovers: Vec<_> = std::fs::read_dir(served.0.join("Lumen"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind");

        // A second push changes nothing that is already the same.
        let before = std::fs::metadata(served.0.join("Lumen/long.wav"))
            .unwrap()
            .modified()
            .unwrap();
        remote::runtime()
            .block_on(push(&server, std::slice::from_ref(&src)))
            .unwrap();
        let after = std::fs::metadata(served.0.join("Lumen/long.wav"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "an unchanged file was sent again");

        // Into an existing folder, a file keeps its name.
        std::fs::create_dir_all(served.0.join("Inbox")).unwrap();
        let single = local
            .0
            .join("Lumen/long.wav")
            .to_string_lossy()
            .into_owned();
        remote::runtime()
            .block_on(push(&server, &[single, "/Inbox".into()]))
            .unwrap();
        assert!(served.0.join("Inbox/long.wav").is_file());
    }

    #[test]
    fn mv_and_rm_change_the_library() {
        let served = TempDir::new("manage");
        tree(&served.0);
        let addr = spawn_for_test(&served.0, Some("k"));
        let server = Server::open(&format!("tuneterm://k@{addr}")).unwrap();
        let rt = remote::runtime();

        rt.block_on(mv(
            &server,
            &["/Lumen/long.wav".into(), "/Other/long.wav".into()],
        ))
        .unwrap();
        assert!(served.0.join("Other/long.wav").is_file());
        assert!(!served.0.join("Lumen/long.wav").exists());

        // Into itself, and onto something that exists, are refused.
        assert!(
            rt.block_on(mv(&server, &["/Lumen".into(), "/Lumen/x".into()]))
                .is_err()
        );
        assert!(
            rt.block_on(mv(&server, &["/Other".into(), "/Lumen".into()]))
                .is_err()
        );

        // A folder with things in it needs -r.
        assert!(rt.block_on(rm(&server, &["/Lumen".into()], false)).is_err());
        assert!(served.0.join("Lumen").is_dir());
        rt.block_on(rm(&server, &["/Lumen".into()], true)).unwrap();
        assert!(!served.0.join("Lumen").exists());

        assert!(rt.block_on(rm(&server, &["/".into()], true)).is_err());
        assert!(served.0.is_dir());
    }

    #[test]
    fn remote_paths_are_normalised() {
        assert_eq!(wire("/Lumen/"), "Lumen");
        assert_eq!(join("/", "a.mp3"), "a.mp3");
        assert_eq!(join("/Lumen", "2002/a.mp3"), "Lumen/2002/a.mp3");
    }

    #[test]
    fn sizes_read_like_ls() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(7 * 1024 * 1024 + 400 * 1024), "7.4 MB");
    }

    #[test]
    fn options_parse() {
        let options = Options::parse(&[
            "-r".into(),
            "/x".into(),
            "--server".into(),
            "tuneterm://nas".into(),
        ])
        .unwrap();
        assert!(options.recursive);
        assert_eq!(options.positional, ["/x"]);
        assert_eq!(options.server.as_deref(), Some("tuneterm://nas"));
        assert!(Options::parse(&["--nope".into()]).is_err());
        assert!(Options::parse(&["--server".into()]).is_err());
    }
}
