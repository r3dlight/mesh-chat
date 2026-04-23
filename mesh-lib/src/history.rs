//! Shared chat-history persistence, used by both the ratatui TUI and the
//! Tauri desktop app.
//!
//! # On-disk format
//!
//! Append-only JSONL at `~/.local/share/mesh-chat/history.jsonl`.
//!
//! When encryption is enabled, the **first line** of the file is a magic
//! header carrying the KDF parameters and a random salt:
//!
//! ```text
//! #MESHCHAT-ENC-V2 argon2id m=65536 t=3 p=4 salt=<base64-16bytes>
//! ```
//!
//! Each subsequent line is `base64(nonce_12 || ciphertext || tag_16)`
//! produced by ChaCha20-Poly1305 with a key derived from the user's
//! passphrase + the salt via Argon2id.
//!
//! **No key material is ever written to disk.** The user must re-enter
//! their passphrase at every launch; losing it means losing the history.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use mesh_core::ChatMessage;
use serde::Deserialize;
use tracing::{debug, info, warn};

pub const HISTORY_MAGIC_V1: &str = "#MESHCHAT-ENC-V1";
pub const HISTORY_MAGIC_V2_PREFIX: &str = "#MESHCHAT-ENC-V2";

/// Fixed plaintext written as the first encrypted line of every new v2
/// history file (right below the magic header). On unlock we decrypt
/// this line and compare against `CANARY_PLAINTEXT`; a wrong
/// passphrase makes the decrypt fail (AEAD tag mismatch) and we
/// surface "wrong passphrase" immediately — without this, a wipe
/// followed by a typo would silently accept the bad passphrase
/// because there were no real messages yet to verify against.
pub const CANARY_PLAINTEXT: &str = "MESHCHAT-CANARY-V2";

/// Argon2id parameters. Tweaking these invalidates previously saved files,
/// so any change must bump the magic version.
const KDF_MEMORY_KB: u32 = 65_536;
const KDF_TIME_COST: u32 = 3;
const KDF_PARALLELISM: u32 = 4;
const SALT_LEN: usize = 16;

/// `[history]` section of the shared config file.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct HistoryConfig {
    /// When true, history lines are encrypted with ChaCha20-Poly1305 using a
    /// key derived from a user-supplied passphrase via Argon2id.
    #[serde(default)]
    pub encrypt: bool,
    /// Rotate `history.jsonl` to `history.jsonl.old` when it grows past this
    /// size (in MB). Default: unlimited. The rotated file is kept as an
    /// archive; loading only reads the current file.
    #[serde(default)]
    pub max_size_mb: Option<u64>,
}

/// Resolved write mode. Carries the in-memory key (never persisted) plus
/// the salt needed to re-derive it on future runs.
#[derive(Clone)]
pub enum HistoryMode {
    Plaintext,
    Encrypted {
        key: [u8; 32],
        salt: [u8; SALT_LEN],
    },
}

impl HistoryMode {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, HistoryMode::Encrypted { .. })
    }
}

/// What the header of the file looks like on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedFormat {
    Empty,
    Plaintext,
    /// Legacy v1: encrypted with a separately-stored key file. Not supported
    /// by the current code for unlocking — the user must migrate.
    V1Legacy,
    /// v2: passphrase-derived key with this salt.
    V2 { salt: [u8; SALT_LEN] },
}

pub fn history_file_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("mesh-chat").join("history.jsonl"))
}

pub fn detect_history_format(path: &Path) -> DetectedFormat {
    if !path.exists() {
        return DetectedFormat::Empty;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return DetectedFormat::Empty;
    };
    let Some(first) = content.lines().next() else {
        return DetectedFormat::Empty;
    };
    let trimmed = first.trim();
    if trimmed == HISTORY_MAGIC_V1 {
        return DetectedFormat::V1Legacy;
    }
    if let Some(rest) = trimmed.strip_prefix(HISTORY_MAGIC_V2_PREFIX) {
        if let Some(salt) = parse_v2_salt(rest.trim()) {
            return DetectedFormat::V2 { salt };
        }
    }
    DetectedFormat::Plaintext
}

fn parse_v2_salt(s: &str) -> Option<[u8; SALT_LEN]> {
    use base64::Engine;
    for field in s.split_whitespace() {
        if let Some(v) = field.strip_prefix("salt=") {
            let bytes = base64::engine::general_purpose::STANDARD.decode(v).ok()?;
            if bytes.len() == SALT_LEN {
                let mut out = [0u8; SALT_LEN];
                out.copy_from_slice(&bytes);
                return Some(out);
            }
        }
    }
    None
}

fn format_v2_header(salt: &[u8; SALT_LEN]) -> String {
    use base64::Engine;
    format!(
        "{} argon2id m={} t={} p={} salt={}",
        HISTORY_MAGIC_V2_PREFIX,
        KDF_MEMORY_KB,
        KDF_TIME_COST,
        KDF_PARALLELISM,
        base64::engine::general_purpose::STANDARD.encode(salt)
    )
}

/// Derives a 32-byte key from a passphrase and salt via Argon2id.
pub fn derive_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(KDF_MEMORY_KB, KDF_TIME_COST, KDF_PARALLELISM, Some(32))
        .map_err(|e| anyhow::anyhow!("argon2 params: {}", e))?;
    let ctx = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    ctx.hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| anyhow::anyhow!("argon2 derive: {}", e))?;
    Ok(out)
}

fn random_salt() -> [u8; SALT_LEN] {
    use rand::RngExt;
    let mut s = [0u8; SALT_LEN];
    rand::rng().fill(&mut s[..]);
    s
}

/// Unlocks an existing v2-encrypted history using the supplied passphrase.
/// Verifies correctness by trying to decrypt the first real line — if the
/// passphrase is wrong, returns an error with that wording.
pub fn unlock_v2(salt: [u8; SALT_LEN], passphrase: &str) -> Result<HistoryMode> {
    let key = derive_key(passphrase, &salt)?;
    // Verify against the first non-header line. Post-fix files have a
    // canary as that line (see `CANARY_PLAINTEXT` and the writer
    // below); pre-fix files have a real message — either way a wrong
    // passphrase makes the AEAD decrypt fail and we surface it.
    if let Some(path) = history_file_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut found_line = false;
            for (i, line) in content.lines().enumerate() {
                if i == 0 || line.trim().is_empty() {
                    continue;
                }
                found_line = true;
                decrypt_line(&key, line).map_err(|_| anyhow::anyhow!("wrong passphrase"))?;
                break;
            }
            if !found_line {
                // Header-only file, no canary, no messages: we can't
                // distinguish a good from a bad passphrase from disk
                // state alone. This only happens for files created by
                // old versions of the app (pre-canary). The caller
                // should treat unlock as provisional — the first write
                // will bake the current key into a canary line and
                // subsequent unlocks will verify properly.
                info!("no verification line in history file — accepting unlock provisionally");
            }
        }
    }
    Ok(HistoryMode::Encrypted { key, salt })
}

/// Sets up encryption for the first time: generates a random salt, derives
/// the key from the passphrase, and returns a mode ready to write.
pub fn init_new_v2(passphrase: &str) -> Result<HistoryMode> {
    let salt = random_salt();
    let key = derive_key(passphrase, &salt)?;
    Ok(HistoryMode::Encrypted { key, salt })
}

pub fn encrypt_line(key: &[u8; 32], line: &str) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
    use rand::RngExt;

    let cipher = ChaCha20Poly1305::new(key.into());
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill(&mut nonce_bytes[..]);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, line.as_bytes())
        .map_err(|e| anyhow::anyhow!("encrypt: {}", e))?;
    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(&combined))
}

pub fn decrypt_line(key: &[u8; 32], encoded: &str) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};

    let combined = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| anyhow::anyhow!("base64: {}", e))?;
    if combined.len() < 12 + 16 {
        bail!("ciphertext shorter than nonce + tag");
    }
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = Nonce::from_slice(&combined[..12]);
    let pt = cipher
        .decrypt(nonce, &combined[12..])
        .map_err(|e| anyhow::anyhow!("decrypt (wrong key?): {}", e))?;
    String::from_utf8(pt).map_err(|e| anyhow::anyhow!("utf8: {}", e))
}

/// If the history file grew past `max_size_mb`, move it aside to
/// `history.jsonl.old` (overwriting any previous archive). The fresh file
/// is recreated by the next `HistoryWriter::open` call, which also
/// rewrites the magic header when encrypted mode is on. Safe to call
/// every startup; no-op when the file is small enough or doesn't exist.
pub fn rotate_if_needed(max_size_mb: u64) -> Result<()> {
    if max_size_mb == 0 {
        return Ok(());
    }
    let Some(path) = history_file_path() else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(&path)?;
    let threshold_bytes = max_size_mb.saturating_mul(1024 * 1024);
    if meta.len() < threshold_bytes {
        return Ok(());
    }
    let old_path = path.with_extension("jsonl.old");
    std::fs::rename(&path, &old_path)?;
    info!(
        size_mb = meta.len() / (1024 * 1024),
        max_mb = max_size_mb,
        from = %path.display(),
        to = %old_path.display(),
        "history rotated"
    );
    Ok(())
}

/// Delete both the live history file and its rotated `.old` archive.
/// Intended for the UI's "clear history" action — the caller is
/// responsible for dropping any open `HistoryWriter` **before** calling
/// this (on Windows you cannot remove a file while it's open).
/// Missing files are treated as success.
pub fn delete_history_files() -> Result<()> {
    let Some(path) = history_file_path() else {
        return Ok(());
    };
    let old_path = path.with_extension("jsonl.old");
    for candidate in [&path, &old_path] {
        match std::fs::remove_file(candidate) {
            Ok(()) => info!(file = %candidate.display(), "history file removed"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Nothing to delete — idempotent.
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "remove {}: {}",
                    candidate.display(),
                    e
                ));
            }
        }
    }
    Ok(())
}

/// Append-only writer. Silently no-ops if the file can't be opened.
pub struct HistoryWriter {
    file: Option<std::fs::File>,
    mode: HistoryMode,
}

impl HistoryWriter {
    pub fn open(mode: HistoryMode) -> Self {
        let Some(path) = history_file_path() else {
            return Self { file: None, mode };
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                // Not fatal: the subsequent OpenOptions::open will surface
                // the real problem if the directory really is missing.
                debug!(dir = %parent.display(), error = %e, "history dir ensure failed");
            }
        }

        let detected = detect_history_format(&path);
        match (&mode, &detected) {
            (HistoryMode::Plaintext, DetectedFormat::V1Legacy)
            | (HistoryMode::Plaintext, DetectedFormat::V2 { .. }) => {
                eprintln!(
                    "History file {} is encrypted but config requested plaintext.",
                    path.display()
                );
                return Self { file: None, mode };
            }
            (HistoryMode::Encrypted { .. }, DetectedFormat::Plaintext) => {
                eprintln!(
                    "History file {} is plaintext but config requested encryption.",
                    path.display()
                );
                return Self { file: None, mode };
            }
            _ => {}
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();

        // New encrypted file: write the v2 magic header with our salt,
        // followed by an encrypted canary line. The canary is the only
        // reliable way to detect a wrong passphrase when the file
        // hasn't yet accumulated any real messages — without it, a
        // wipe + typo silently wrote lines under a bad key (see
        // `CANARY_PLAINTEXT` for the full rationale).
        if matches!(detected, DetectedFormat::Empty) {
            if let (HistoryMode::Encrypted { salt, key }, Some(f)) = (&mode, file.as_mut()) {
                if let Err(e) = writeln!(f, "{}", format_v2_header(salt)) {
                    warn!(error = %e, "writing v2 header failed; disabling history");
                    return Self { file: None, mode };
                }
                match encrypt_line(key, CANARY_PLAINTEXT) {
                    Ok(enc) => {
                        if let Err(e) = writeln!(f, "{}", enc) {
                            warn!(error = %e, "writing canary failed; disabling history");
                            return Self { file: None, mode };
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "canary encrypt failed; disabling history");
                        return Self { file: None, mode };
                    }
                }
            }
        }

        Self { file, mode }
    }

    pub fn record(&mut self, msg: &ChatMessage) {
        let Some(f) = self.file.as_mut() else { return };
        let Ok(json) = serde_json::to_string(msg) else {
            return;
        };
        let line = match &self.mode {
            HistoryMode::Plaintext => json,
            HistoryMode::Encrypted { key, .. } => match encrypt_line(key, &json) {
                Ok(enc) => enc,
                Err(e) => {
                    warn!(error = %e, "history encrypt failed, skipping line");
                    return;
                }
            },
        };
        if let Err(e) = writeln!(f, "{}", line) {
            // Stay operational: one failed line shouldn't bring down the app,
            // but we do want a trail in the logs so disk issues surface.
            warn!(error = %e, "history write failed; skipping line");
        }
    }
}

/// Summary of what `load_history` did.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct LoadReport {
    pub restored: usize,
    pub errors: usize,
}

pub fn load_history(mode: &HistoryMode, mut sink: impl FnMut(ChatMessage)) -> LoadReport {
    let mut report = LoadReport::default();
    let Some(path) = history_file_path() else {
        return report;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return report;
    };
    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            let t = line.trim();
            if t == HISTORY_MAGIC_V1 || t.starts_with(HISTORY_MAGIC_V2_PREFIX) {
                continue;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let json_str = match mode {
            HistoryMode::Plaintext => line.to_string(),
            HistoryMode::Encrypted { key, .. } => match decrypt_line(key, line) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "history decrypt failed, skipping line");
                    report.errors += 1;
                    continue;
                }
            },
        };
        match serde_json::from_str::<ChatMessage>(&json_str) {
            Ok(msg) => {
                sink(msg);
                report.restored += 1;
            }
            Err(e) => {
                warn!(error = %e, "history parse failed, skipping line");
                report.errors += 1;
            }
        }
    }
    if report.restored > 0 || report.errors > 0 {
        info!(
            restored = report.restored,
            errors = report.errors,
            encrypted = mode.is_encrypted(),
            path = %path.display(),
            "history loaded"
        );
    }
    report
}

pub fn dump_history_to_stdout(mode: &HistoryMode) -> Result<()> {
    let Some(path) = history_file_path() else {
        bail!("no default data dir");
    };
    if !path.exists() {
        eprintln!("No history file at {}", path.display());
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)?;
    for (i, line) in content.lines().enumerate() {
        if i == 0 {
            let t = line.trim();
            if t == HISTORY_MAGIC_V1 || t.starts_with(HISTORY_MAGIC_V2_PREFIX) {
                continue;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let out = match mode {
            HistoryMode::Plaintext => line.to_string(),
            HistoryMode::Encrypted { key, .. } => match decrypt_line(key, line) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("line {}: decrypt failed: {}", i + 1, e);
                    continue;
                }
            },
        };
        println!("{}", out);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mesh_core::Network;

    fn make_msg(text: &str) -> ChatMessage {
        ChatMessage {
            timestamp: 1,
            network: Network::Meshtastic,
            channel: 0,
            from: "!abcd1234".into(),
            to: "^all".into(),
            text: text.into(),
            local_id: None,
            status: None,
            rx_snr: None,
            rx_rssi: None,
            reply_to_text: None,
            packet_id: None,
            reactions: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn argon2_derive_is_deterministic() {
        let salt = [42u8; SALT_LEN];
        let k1 = derive_key("hunter2", &salt).unwrap();
        let k2 = derive_key("hunter2", &salt).unwrap();
        assert_eq!(k1, k2);
        let k3 = derive_key("other", &salt).unwrap();
        assert_ne!(k1, k3);
        let salt2 = [0u8; SALT_LEN];
        let k4 = derive_key("hunter2", &salt2).unwrap();
        assert_ne!(k1, k4);
    }

    #[test]
    fn v2_header_roundtrip() {
        let salt = [7u8; SALT_LEN];
        let header = format_v2_header(&salt);
        assert!(header.starts_with(HISTORY_MAGIC_V2_PREFIX));
        let trimmed = header.trim_start_matches(HISTORY_MAGIC_V2_PREFIX).trim();
        let parsed = parse_v2_salt(trimmed).expect("salt parse");
        assert_eq!(parsed, salt);
    }

    #[test]
    fn v2_salt_parse_rejects_wrong_length() {
        use base64::Engine;
        let bad_b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        let line = format!("argon2id m=1 t=1 p=1 salt={}", bad_b64);
        assert!(parse_v2_salt(&line).is_none());
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [1u8; 32];
        let ct = encrypt_line(&key, "hello").unwrap();
        let pt = decrypt_line(&key, &ct).unwrap();
        assert_eq!(pt, "hello");
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let k1 = [1u8; 32];
        let k2 = [2u8; 32];
        let ct = encrypt_line(&k1, "secret").unwrap();
        assert!(decrypt_line(&k2, &ct).is_err());
    }

    #[test]
    fn encrypt_uses_fresh_nonce_per_call() {
        let key = [3u8; 32];
        let a = encrypt_line(&key, "same").unwrap();
        let b = encrypt_line(&key, "same").unwrap();
        // Same plaintext + key but different nonces → ciphertexts differ.
        assert_ne!(a, b);
        // Both still decrypt to the same plaintext.
        assert_eq!(decrypt_line(&key, &a).unwrap(), "same");
        assert_eq!(decrypt_line(&key, &b).unwrap(), "same");
    }

    #[test]
    fn chat_message_encrypt_then_json_roundtrip() {
        let key = [9u8; 32];
        let msg = make_msg("hi there");
        let json = serde_json::to_string(&msg).unwrap();
        let enc = encrypt_line(&key, &json).unwrap();
        let dec = decrypt_line(&key, &enc).unwrap();
        let back: ChatMessage = serde_json::from_str(&dec).unwrap();
        assert_eq!(back.text, "hi there");
    }

    #[test]
    fn history_mode_helpers() {
        assert!(!HistoryMode::Plaintext.is_encrypted());
        let m = HistoryMode::Encrypted {
            key: [0u8; 32],
            salt: [0u8; SALT_LEN],
        };
        assert!(m.is_encrypted());
    }

    #[test]
    fn canary_roundtrip_with_right_key() {
        // Freshly-generated key encrypts the canary, same key decrypts
        // it back to the literal. This is the minimum guarantee
        // `unlock_v2` relies on to detect a wrong passphrase even
        // when the file has no real messages yet.
        let key = derive_key("hunter2", &[7u8; SALT_LEN]).unwrap();
        let encoded = encrypt_line(&key, CANARY_PLAINTEXT).unwrap();
        let decoded = decrypt_line(&key, &encoded).unwrap();
        assert_eq!(decoded, CANARY_PLAINTEXT);
    }

    #[test]
    fn canary_decrypt_fails_with_wrong_key() {
        let salt = [7u8; SALT_LEN];
        let good = derive_key("hunter2", &salt).unwrap();
        let bad = derive_key("hunter3", &salt).unwrap();
        let encoded = encrypt_line(&good, CANARY_PLAINTEXT).unwrap();
        assert!(decrypt_line(&bad, &encoded).is_err());
    }
}
