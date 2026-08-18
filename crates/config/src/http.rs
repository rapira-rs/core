use anyhow::bail;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::config_relative;
use crate::listen::Listen;

#[derive(Debug)]
pub struct HttpSettings {
    pub listen: Listen,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub write_timeout: std::time::Duration,
    pub unsafe_field_names: UnsafeFieldNames,
    pub uploads: UploadSettings,
    /// `[http.sendfile].root`; None = the entrypoint's directory.
    pub sendfile_root: Option<PathBuf>,
}

#[derive(Debug)]
pub struct UploadSettings {
    pub dir: PathBuf,
    pub max_file_size: u64,
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
    pub max_part_headers: usize,
}

/// The `HTTP_*` mapping rewrites `-` to `_` and PHP rewrites `.` to `_`, so `X_Forwarded_For` and `X.Forwarded.For` both land on `HTTP_X_FORWARDED_FOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnsafeFieldNames {
    #[default]
    Drop,
    Reject,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpSection {
    pub(crate) listen: Option<String>,
    pub(crate) server_name: Option<String>,
    pub(crate) server_port: Option<u16>,
    pub(crate) max_body_size_mb: Option<usize>,
    pub(crate) write_timeout_secs: Option<u64>,
    pub(crate) unsafe_field_names: Option<UnsafeFieldNames>,
    /// Option so presence is observable: the table is rejected outside dispatcher mode.
    pub(crate) uploads: Option<UploadsSection>,
    #[serde(default)]
    pub(crate) sendfile: SendfileSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendfileSection {
    pub(crate) root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadsSection {
    dir: Option<String>,
    max_file_size_mb: Option<u64>,
    max_field_size_kb: Option<usize>,
    max_files: Option<usize>,
    max_parts: Option<usize>,
    max_part_headers: Option<usize>,
}

pub(crate) fn resolve_uploads(
    section: UploadsSection,
    config_dir: Option<&Path>,
) -> anyhow::Result<UploadSettings> {
    let dir = match section.dir.filter(|d| !d.is_empty()) {
        Some(d) => config_relative(config_dir, &d)?,
        None => std::env::temp_dir(),
    };
    let max_file_size_mb = section.max_file_size_mb.unwrap_or(2);
    if max_file_size_mb == 0 {
        bail!("http.uploads.max_file_size_mb must be at least 1");
    }
    let max_file_size = max_file_size_mb.checked_mul(1024 * 1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_file_size_mb {max_file_size_mb} is too large")
    })?;
    let max_field_size_kb = section.max_field_size_kb.unwrap_or(256);
    if max_field_size_kb == 0 {
        bail!("http.uploads.max_field_size_kb must be at least 1");
    }
    let max_field_size = max_field_size_kb.checked_mul(1024).ok_or_else(|| {
        anyhow::anyhow!("http.uploads.max_field_size_kb {max_field_size_kb} is too large")
    })?;
    let max_parts = section.max_parts.unwrap_or(1024);
    if max_parts == 0 {
        bail!("http.uploads.max_parts must be at least 1");
    }
    let max_part_headers = section.max_part_headers.unwrap_or(32);
    if max_part_headers == 0 {
        bail!("http.uploads.max_part_headers must be at least 1");
    }
    let max_files = section.max_files.unwrap_or(20);
    if max_files == 0 {
        bail!("http.uploads.max_files must be at least 1");
    }
    Ok(UploadSettings {
        dir,
        max_file_size,
        max_field_size,
        max_files,
        max_parts,
        max_part_headers,
    })
}
