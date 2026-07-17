#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

struct PhpBuild {
    /// clang/cc `-I` include flags, space-separated (as `php-config --includes` emits).
    includes: String,
    /// directories to search for the PHP link library.
    lib_dirs: Vec<String>,
    /// the PHP link library name (`php` on Unix, `php8ts`/`php8` on Windows).
    lib_name: String,
    abi: PhpAbi,
}

struct PhpAbi {
    version: (u32, u32),
    zts: bool,
    debug: bool,
}

fn main() -> anyhow::Result<()> {
    println!("cargo:rustc-check-cfg=cfg(php84, php85, php_zts, php_debug)");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");
    println!("cargo:rerun-if-env-changed=PHP_DEVEL_DIR");
    println!("cargo:rerun-if-env-changed=RAPIRA_ALLOWED_BINDINGS");

    let php = discover_php()?;

    for dir in &php.lib_dirs {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib={}", php.lib_name);

    for (vmajor, vminor) in [(8, 4), (8, 5)] {
        if php.abi.version >= (vmajor, vminor) {
            println!("cargo:rustc-cfg=php{vmajor}{vminor}");
        }
    }
    if php.abi.zts {
        println!("cargo:rustc-cfg=php_zts");
    }
    if php.abi.debug {
        println!("cargo:rustc-cfg=php_debug");
    }

    let includes: Vec<&str> = php
        .includes
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I"))
        .collect();

    let mut c = cc::Build::new();
    c.file("wrapper.c").file("module.c");
    if php.abi.zts {
        c.define("ZTS", None);
    }
    for d in &includes {
        c.include(d);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(php.includes.split_whitespace());
    if php.abi.zts {
        bindings = bindings.clang_arg("-DZTS");
    }
    bindings = bindings.layout_tests(true);

    for binding in ALLOWED_BINDINGS {
        bindings = bindings
            .allowlist_function(binding)
            .allowlist_type(binding)
            .allowlist_var(binding);
    }

    if let Ok(extra) = env::var("RAPIRA_ALLOWED_BINDINGS") {
        for binding in extra.split(',') {
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

fn parse_version(v: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    Ok((major, minor))
}

#[cfg(unix)]
fn discover_php() -> anyhow::Result<PhpBuild> {
    let includes = php_config("--includes")?;
    let prefix = php_config("--prefix")?;
    let version = parse_version(&php_config("--version")?)?;
    let bin = resolve_php_binary();
    let zts = detect_zts(&bin)?;
    let debug = detect_debug(&bin)?;
    Ok(PhpBuild {
        includes,
        lib_dirs: vec![format!("{prefix}/lib"), format!("{prefix}/lib64")],
        lib_name: "php".to_string(),
        abi: PhpAbi {
            version,
            zts,
            debug,
        },
    })
}

// `php-config --php-binary` can name a path that doesn't exist (the Homebrew zts keg reports
// one), so fall back to `php` on PATH, which setup-php/Homebrew also install.
#[cfg(unix)]
fn resolve_php_binary() -> String {
    if let Ok(bin) = php_config("--php-binary")
        && std::path::Path::new(&bin).exists()
    {
        return bin;
    }
    "php".to_string()
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

fn detect_debug(php_binary: &str) -> anyhow::Result<bool> {
    let out = Command::new(php_binary)
        .arg("-i")
        .output()
        .context("running php -i")?;

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.split("Debug Build => ").nth(1))
        .map(|v| v.trim() == "yes")
        .unwrap_or(false))
}

#[cfg(unix)]
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

#[cfg(windows)]
fn discover_php() -> anyhow::Result<PhpBuild> {
    let root = env::var("PHP_DEVEL_DIR")
        .or_else(|_| env::var("PHP_SDK_PATH"))
        .context("set PHP_DEVEL_DIR to the extracted PHP devel pack root")?;
    let root = PathBuf::from(root);
    let inc = root.join("include");
    let include_dirs = ["", "main", "Zend", "TSRM", "ext", "win32"];
    let includes = include_dirs
        .iter()
        .map(|d| format!("-I{}", inc.join(d).display()))
        .collect::<Vec<_>>()
        .join(" ");
    let lib_dir = root.join("lib");
    let version = parse_version(&read_php_version(&inc)?)?;

    // `php` is on PATH on both Windows CI legs (setup-php, or the --enable-cli build), so detect
    // the ABI at runtime as on Unix. If none runs, infer ZTS from the shipped import lib and
    // assume a non-debug build.
    let (zts, debug) = match detect_zts("php") {
        Ok(zts) => (zts, detect_debug("php").unwrap_or(false)),
        Err(_) => (
            lib_dir.join(format!("php{}ts.lib", version.0)).exists(),
            false,
        ),
    };
    let lib_name = if zts {
        format!("php{}ts", version.0)
    } else {
        format!("php{}", version.0)
    };
    anyhow::ensure!(
        lib_dir.join(format!("{lib_name}.lib")).exists(),
        "expected {lib_name}.lib in {}",
        lib_dir.display()
    );
    Ok(PhpBuild {
        includes,
        lib_dirs: vec![lib_dir.display().to_string()],
        lib_name,
        abi: PhpAbi {
            version,
            zts,
            debug,
        },
    })
}

#[cfg(windows)]
fn read_php_version(include: &std::path::Path) -> anyhow::Result<String> {
    let header = include.join("main").join("php_version.h");
    let text = std::fs::read_to_string(&header)
        .with_context(|| format!("reading {}", header.display()))?;
    let grab = |name: &str| {
        let needle = format!("#define {name} ");
        text.lines()
            .find_map(|l| l.strip_prefix(needle.as_str()))
            .map(|v| v.trim().to_string())
    };
    let major = grab("PHP_MAJOR_VERSION").context("PHP_MAJOR_VERSION not found")?;
    let minor = grab("PHP_MINOR_VERSION").context("PHP_MINOR_VERSION not found")?;
    Ok(format!("{major}.{minor}"))
}
