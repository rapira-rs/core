#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

fn main() -> anyhow::Result<()> {
    println!("cargo:rustc-check-cfg=cfg(php84, php85, php_zts, php_debug)");

    let php_includes = php_config("--includes")?;
    let php_prefix = php_config("--prefix")?;
    // where is to search libs
    println!("cargo:rustc-link-search=native={php_prefix}/lib");
    println!("cargo:rustc-link-search=native={php_prefix}/lib64");
    println!("cargo:rustc-link-lib=dylib=php");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");

    let abi = detect_php_abi()?;

    for (vmajor, vminor) in [(8, 4), (8, 5)] {
        if abi.version >= (vmajor, vminor) {
            println!("cargo:rustc-cfg=php{vmajor}{vminor}");
        }
    }

    if abi.zts {
        println!("cargo:rustc-cfg=php_zts");
    }

    if abi.debug {
        println!("cargo:rustc-cfg=php_debug");
    }

    // compile c code
    let includes: Vec<&str> = php_includes
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I"))
        .collect();

    let mut c = cc::Build::new();
    c.file("wrapper.c").file("module.c");
    if abi.zts {
        c.define("ZTS", None);
    }
    for d in &includes {
        c.include(d);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(php_includes.split_whitespace());
    if abi.zts {
        bindings = bindings.clang_arg("-DZTS");
    }

    bindings = bindings.layout_tests(true); // size/offset asserts

    for binding in ALLOWED_BINDINGS {
        bindings = bindings
            .allowlist_function(binding)
            .allowlist_type(binding)
            .allowlist_var(binding);
    }

    if let Ok(extra) = env::var("RAPIRA_ALLOWED_BINDINGS") {
        for binding in extra.split(",") {
            bindings = bindings
                .allowlist_function(binding)
                .allowlist_type(binding)
                .allowlist_var(binding);
        }
    }

    bindings
        .generate()?
        .write_to_file(PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs"))?;

    for f in ["wrapper.h", "module.c", "wrapper.c", "allowed_bindings.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }

    Ok(())
}

fn detect_debug(php_binary: &str) -> anyhow::Result<bool> {
    let out = Command::new(php_binary)
        .arg("-i")
        .output()
        .context("running php -i")?;

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.split("Debug Build -> ").nth(1))
        .map(|v| v.trim() == "yes")
        .unwrap_or(false))
}

fn php_config(arg: &str) -> anyhow::Result<String> {
    let bin = env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let out = Command::new(&bin)
        .arg(arg)
        .output()
        .with_context(|| format!("running {bin} {arg}"))?;

    anyhow::ensure!(
        out.status.success(),
        "{bin} {arg} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn detect_zts(php_binary: &str) -> anyhow::Result<bool> {
    if let Ok(out) = Command::new(php_binary)
        .args(["-r", "echo PHP_ZTS;"])
        .output()
    {
        match String::from_utf8_lossy(&out.stdout).trim() {
            "1" => return Ok(true),
            "0" => return Ok(false),
            _ => {} // usually not possible, but fallback to `php -i` if it happens
        }
    }
    let out = Command::new(php_binary)
        .arg("-i")
        .output()
        .context("running `php -i`")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.split("Thread Safety => ").nth(1))
        .map(|v| v.trim() == "enabled")
        .context("could not determine Thread Safety from `php -i`")
}

struct PhpAbi {
    version: (u32, u32),
    zts: bool,
    debug: bool,
}

fn detect_php_abi() -> anyhow::Result<PhpAbi> {
    let version = php_config("--version")?;
    let mut it = version.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    let bin = php_config("--php-binary")?;
    let zts = detect_zts(&bin)?;
    let debug = detect_debug(&bin)?;
    Ok(PhpAbi {
        version: (major, minor),
        zts,
        debug,
    })
}
