//! Per-user overrides: custom names and favorites for nodes.
//!
//! Stored in a JSON file under `dirs::data_dir()/mesh-chat/aliases.json`
//! because it's machine-written (not user-edited by hand) and because
//! we want it separate from the human-curated `config.toml`. The file
//! is rewritten atomically on every mutation (tmp + rename) so a crash
//! mid-write doesn't leave a half-file.
//!
//! Schema stays intentionally tiny: a `HashMap<node_id, alias>` and a
//! `Vec<node_id>` (treated as a set, order-preserving for predictable
//! sidebar ordering).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// On-disk shape. Stable; additive changes only (new fields must
/// default-serde).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Aliases {
    /// Maps `node_id` (e.g. `"!49b5b33c"` for Meshtastic or a 12-hex
    /// prefix for Meshcore) to the user's custom display name. An empty
    /// string or missing entry means "use whatever the mesh advertises".
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Pinned / favorite nodes. DMs with these peers float to the top
    /// of the sidebar regardless of recency.
    #[serde(default)]
    pub favorites: Vec<String>,
    /// Last backend the user picked in the UI Connect card. Overrides
    /// the `[general] network` value in `config.toml` when set so the
    /// user doesn't have to hand-edit TOML just because they reflashed
    /// their radio. `"meshtastic"` / `"meshcore"` (lowercase).
    #[serde(default)]
    pub preferred_network: Option<String>,
}

impl Aliases {
    /// Look up a custom alias, if any.
    pub fn get(&self, node_id: &str) -> Option<&str> {
        self.aliases.get(node_id).map(String::as_str)
    }

    /// Set (or clear, if `alias` is `None` or empty after trim) the
    /// custom name for `node_id`.
    pub fn set(&mut self, node_id: impl Into<String>, alias: Option<String>) {
        let node_id = node_id.into();
        match alias {
            Some(s) if !s.trim().is_empty() => {
                self.aliases.insert(node_id, s.trim().to_string());
            }
            _ => {
                self.aliases.remove(&node_id);
            }
        }
    }

    pub fn is_favorite(&self, node_id: &str) -> bool {
        self.favorites.iter().any(|id| id == node_id)
    }

    pub fn set_favorite(&mut self, node_id: impl Into<String>, fav: bool) {
        let node_id = node_id.into();
        let pos = self.favorites.iter().position(|id| id == &node_id);
        match (pos, fav) {
            (None, true) => self.favorites.push(node_id),
            (Some(i), false) => {
                self.favorites.remove(i);
            }
            _ => {}
        }
    }
}

pub fn aliases_file_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("mesh-chat").join("aliases.json"))
}

/// Load the aliases file if it exists and parses cleanly. On any failure
/// returns an empty `Aliases` — the data is purely cosmetic, losing it
/// never blocks the user from chatting.
pub fn load() -> Aliases {
    let Some(path) = aliases_file_path() else {
        return Aliases::default();
    };
    if !path.exists() {
        return Aliases::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<Aliases>(&content) {
            Ok(a) => {
                debug!(
                    path = %path.display(),
                    aliases = a.aliases.len(),
                    favorites = a.favorites.len(),
                    "aliases loaded"
                );
                a
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "aliases file unparseable; starting fresh");
                Aliases::default()
            }
        },
        Err(e) => {
            warn!(path = %path.display(), error = %e, "aliases read failed; starting fresh");
            Aliases::default()
        }
    }
}

/// Atomically persist `aliases` to disk: write to `aliases.json.tmp`
/// then rename over the real file so a crash mid-write can't leave
/// the file half-populated.
pub fn save(aliases: &Aliases) -> Result<()> {
    let Some(path) = aliases_file_path() else {
        anyhow::bail!("no data_dir — cannot persist aliases");
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all({})", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        let json = serde_json::to_string_pretty(aliases)
            .context("serialize aliases")?;
        f.write_all(json.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    debug!(path = %path.display(), "aliases persisted");
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_alias_trims_whitespace() {
        let mut a = Aliases::default();
        a.set("!abcd1234", Some("  Stef  ".into()));
        assert_eq!(a.get("!abcd1234"), Some("Stef"));
    }

    #[test]
    fn set_alias_to_empty_string_clears_it() {
        let mut a = Aliases::default();
        a.set("!abcd1234", Some("Name".into()));
        assert!(a.get("!abcd1234").is_some());
        a.set("!abcd1234", Some("   ".into()));
        assert!(a.get("!abcd1234").is_none());
    }

    #[test]
    fn set_alias_to_none_clears_it() {
        let mut a = Aliases::default();
        a.set("!abcd1234", Some("Name".into()));
        a.set("!abcd1234", None);
        assert!(a.get("!abcd1234").is_none());
    }

    #[test]
    fn favorite_toggle_is_idempotent() {
        let mut a = Aliases::default();
        a.set_favorite("!one", true);
        a.set_favorite("!one", true);
        assert_eq!(a.favorites.len(), 1);
        a.set_favorite("!one", false);
        a.set_favorite("!one", false);
        assert_eq!(a.favorites.len(), 0);
    }

    #[test]
    fn favorites_preserve_insertion_order() {
        let mut a = Aliases::default();
        a.set_favorite("!one", true);
        a.set_favorite("!two", true);
        a.set_favorite("!three", true);
        assert_eq!(a.favorites, vec!["!one", "!two", "!three"]);
    }

    #[test]
    fn roundtrip_via_json() {
        let mut a = Aliases::default();
        a.set("!abcd1234", Some("Alice".into()));
        a.set_favorite("!abcd1234", true);
        let json = serde_json::to_string(&a).unwrap();
        let back: Aliases = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("!abcd1234"), Some("Alice"));
        assert!(back.is_favorite("!abcd1234"));
    }

    #[test]
    fn unknown_json_fields_tolerated() {
        // Future-proofing: if a later version adds fields, older builds
        // should still parse the file as best they can.
        let json = r#"{"aliases":{"!a":"A"},"favorites":["!a"],"future_field":"ignored"}"#;
        let a: Aliases = serde_json::from_str(json).unwrap();
        assert_eq!(a.get("!a"), Some("A"));
    }
}
