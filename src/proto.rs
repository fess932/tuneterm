//! Types and stubs generated from `proto/tuneterm/v1/library.proto`. The server and
//! the client both build on these, so a change to the protocol that one side does
//! not follow is a compile error rather than a runtime surprise.

#![allow(clippy::all, clippy::pedantic)]

tonic::include_proto!("tuneterm.v1");

/// Largest message either side will accept. tonic's default is 4 MiB, which a
/// big embedded cover or a listing of a few tens of thousands of tracks can pass.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// Size of each piece of a file on the wire, both ways. Large enough that a track is
/// a few dozen messages, small enough that a seek wastes little.
pub const CHUNK: usize = 256 * 1024;

/// Where a server listens unless told otherwise.
pub const DEFAULT_PORT: u16 = 7700;
