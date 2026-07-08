//! `extension.toml`: minimal metadata. The binary `rapira:api-version` stamp is
//! the authoritative version, and the extension's behavior lives entirely in its
//! wasm, so the manifest is just an id.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::path::Path;

/// The manifest as written on disk. `name`/`version`/`api_version` keys are
/// author-facing and ignored by the host (serde skips unknown keys).
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub id: String,
}

impl Manifest {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let manifest: Manifest = toml::from_str(text).context("parsing extension.toml")?;
        if !is_valid_id(&manifest.id) {
            bail!(
                "extension id {:?} must match [a-z][a-z0-9_-]{{0,63}}",
                manifest.id
            );
        }
        Ok(manifest)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }
}

/// Dir-name- and path-safe, since the loader and future registry write `<ext_dir>/<id>`.
fn is_valid_id(id: &str) -> bool {
    id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_manifest() {
        let m = Manifest::parse("id = \"hello\"\nname = \"Hello\"\n").expect("parse");
        assert_eq!(m.id, "hello");
    }

    #[test]
    fn rejects_a_bad_id() {
        for bad in ["Hello", "../etc", "hello/world", "9hello", ""] {
            assert!(
                Manifest::parse(&format!("id = \"{bad}\"")).is_err(),
                "{bad} should be rejected"
            );
        }
    }
}
