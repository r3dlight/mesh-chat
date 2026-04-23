//! Shared `tracing` / `EnvFilter` defaults.
//!
//! The upstream `meshtastic` crate logs every partial serial read at
//! `ERROR` — on a healthy USB-CDC link you still see hundreds of
//! "incomplete packet data" / "could not find header sequence" lines
//! because the host-side reads don't land on a frame boundary. The
//! crate resynchronises on the next `0x94 0xC3` marker, so these are
//! benign; muting them is the right default.

/// Directives applied when `RUST_LOG` is not set. Users can always
/// override by exporting their own filter.
///
/// - `info` for our crates (default level)
/// - `off` for the `stream_buffer` module (drop resync noise)
pub const DEFAULT_FILTER: &str = "info,meshtastic::connections::stream_buffer=off";
