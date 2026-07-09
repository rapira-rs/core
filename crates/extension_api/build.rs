//! Stamps the SDK's version into `$OUT_DIR/version_bytes` as six big-endian bytes
//! (major, minor, patch as `u16`). `lib.rs` embeds them in the guest's
//! `rapira:api-version` custom section, which the host reads before instantiating.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let version = std::env::var("CARGO_PKG_VERSION")?;
    let out_dir = std::env::var("OUT_DIR")?;

    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    let mut next = || -> Result<[u8; 2], Box<dyn std::error::Error>> {
        let part = parts.next().ok_or("version is not major.minor.patch")?;
        Ok(part.parse::<u16>()?.to_be_bytes())
    };
    let (major, minor, patch) = (next()?, next()?, next()?);

    std::fs::write(
        Path::new(&out_dir).join("version_bytes"),
        [major[0], major[1], minor[0], minor[1], patch[0], patch[1]],
    )?;

    Ok(())
}
