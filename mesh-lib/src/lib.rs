//! Shared client-side utilities used by both the ratatui TUI and the Tauri
//! desktop app. Protocol-agnostic; depends on `mesh-core` for shared types.
//!
//! - `config`: shared `config.toml` schema (`[general]`, `[history]`)
//! - `history`: append-only on-disk chat log, optionally encrypted with
//!   ChaCha20-Poly1305
//! - `serial`: list available serial ports for any serial-based mesh
//!   backend

pub mod aliases;
pub mod config;
pub mod history;
pub mod logging;
pub mod serial;
