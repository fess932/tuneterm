// Without the player, the client half of the remote library and most of `library`
// have no caller. They are still compiled, because the server shares them.
#![cfg_attr(not(feature = "player"), allow(dead_code))]

#[cfg(feature = "player")]
mod app;
#[cfg(feature = "player")]
mod cache;
mod cli;
mod config;
#[cfg(feature = "player")]
mod cover;
#[cfg(feature = "player")]
mod feed;
mod library;
#[cfg(feature = "player")]
mod media;
#[cfg(feature = "player")]
mod net;
#[cfg(feature = "player")]
mod player;
mod proto;
mod ratings;
mod remote;
mod server;
#[cfg(feature = "player")]
mod tui;
#[cfg(feature = "player")]
mod ui;
mod worker;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|first| cli::COMMANDS.contains(&first.as_str()))
    {
        return cli::run(args);
    }
    player(args)
}

#[cfg(feature = "player")]
fn player(args: Vec<String>) -> anyhow::Result<()> {
    tui::main(args)
}

/// A build without the player — the Docker image — still answers `--help` and
/// `--version`, and says plainly what it cannot do.
#[cfg(not(feature = "player"))]
fn player(args: Vec<String>) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help") => {
            println!("{}", cli::help());
            Ok(())
        }
        _ => {
            eprintln!("this build has no player; it serves and manages a library.\n");
            eprintln!("{}", cli::help());
            std::process::exit(2);
        }
    }
}
