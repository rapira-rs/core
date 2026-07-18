#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

struct PhpBuild {
    /// include directories (no `-I` prefix; may contain spaces on Windows).
    includes: Vec<String>,
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
    println!("cargo:rustc-check-cfg=cfg(php85, php_zts)");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");
    println!("cargo:rerun-if-env-changed=PHP_DEVEL_DIR");
    println!("cargo:rerun-if-env-changed=PHP_SDK_PATH");

    let php = discover_php()?;

    for dir in &php.lib_dirs {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib={}", php.lib_name);

    if php.abi.version >= (8, 5) {
        println!("cargo:rustc-cfg=php85");
    }
    if php.abi.zts {
        println!("cargo:rustc-cfg=php_zts");
    }

    let win_defs = windows_defines(&php.abi);

    let mut c = cc::Build::new();
    c.file("wrapper.c").file("module.c");
    if php.abi.zts {
        c.define("ZTS", None);
    }
    for &(k, v) in &win_defs {
        c.define(k, Some(v));
    }
    for d in &php.includes {
        c.include(d);
    }
    if cfg!(windows) {
        // Match the PHP DLL's C runtime: /MDd for a --enable-debug build, /MD otherwise. A shim on
        // a different CRT than php8ts.dll gets its own heap and errno/FILE* state, so a buffer
        // allocated inside PHP and freed across the boundary corrupts. cc has no debug-CRT knob
        // (static_crt(false) always emits /MD and c.debug only adds /Z7), so append /MDd
        // explicitly — cl takes the last /M flag (the D9025 override warning is expected).
        c.static_crt(false);
        if php.abi.debug {
            c.flag("-MDd");
        }
        c.debug(php.abi.debug);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(php.includes.iter().map(|d| format!("-I{d}")))
        .clang_args(win_defs.iter().map(|(k, v)| format!("-D{k}={v}")))
        // marks the bindgen/libclang parse for wrapper.h's parse-only rewrites; the real
        // compiler (cl.exe or clang-cl) never sees it
        .clang_arg("-DRAPIRA_BINDGEN=1")
        // php-src master on clang >=19 compiles the Zend VM in tail-call dispatch mode: every
        // opcode handler is a function using the `preserve_none` calling convention, and
        // `zend_op.handler` points to one. bindgen renders C structs field-by-field, so it must
        // emit that pointer's ABI - but 0.72.1 has no token for `preserve_none` and panics.
        // `opaque_type` makes bindgen emit `zend_op` as a byte array of clang's reported
        // size/align instead of its fields, so the handler pointer is never rendered. rapira reads
        // no `zend_op` field, so nothing is lost; a no-op on 8.4/8.5, which have no preserve_none.
        // https://clang.llvm.org/docs/AttributeReference.html#preserve-none
        .opaque_type("_zend_op");
    if php.abi.zts {
        bindings = bindings.clang_arg("-DZTS");
    }
    bindings = bindings.layout_tests(true);
    bindings = macos_sysroot(bindings);

    for binding in ALLOWED_BINDINGS {
        bindings = bindings
            .allowlist_function(binding)
            .allowlist_type(binding)
            .allowlist_var(binding);
    }

    bindings
        .generate()?
        .write_to_file(PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs"))?;

    for f in ["wrapper.h", "module.c", "wrapper.c", "allowed_bindings.rs"] {
        println!("cargo:rerun-if-changed={f}");
    }

    Ok(())
}

// PHP passes these on the compiler command line, never in a header; without ZEND_WIN32 the headers
// #include the Unix-only <zend_config.h> and the build dies. Empty off Windows, where the generated
// php_config.h already carries them.
fn windows_defines(abi: &PhpAbi) -> Vec<(&'static str, &'static str)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    vec![
        ("ZEND_WIN32", "1"),
        ("PHP_WIN32", "1"),
        ("WIN32", "1"),
        ("WINDOWS", "1"),
        ("_WINDOWS", "1"),
        ("_MBCS", "1"),
        ("_USE_MATH_DEFINES", "1"),
        // PHP's headers reference ZEND_DEBUG unconditionally (STANDARD_MODULE_HEADER builds the
        // module struct from it), and on Windows it's only ever a command-line define - so it must
        // always be set: 1 to match a --enable-debug DLL's struct layout, else 0. (The debug CRT
        // /MDd, passed explicitly for a debug build, defines _DEBUG on its own.)
        ("ZEND_DEBUG", if abi.debug { "1" } else { "0" }),
    ]
}

#[cfg(target_os = "macos")]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    // The libclang 19+ that reports the preserve_none convention no longer infers the SDK path, so
    // point it at the active SDK or the parse fails to find <stdlib.h> and friends.
    if let Ok(out) = Command::new("xcrun").args(["--show-sdk-path"]).output()
        && out.status.success()
    {
        let sdk = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !sdk.is_empty() {
            return bindings
                .clang_arg(format!("-isysroot{sdk}"))
                .clang_arg(format!("-I{sdk}/usr/include"));
        }
    }
    bindings
}

#[cfg(not(target_os = "macos"))]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    bindings
}

fn parse_version(v: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    Ok((major, minor))
}

#[cfg(unix)]
fn discover_php() -> anyhow::Result<PhpBuild> {
    // `php-config --includes` emits space-separated `-I` flags; paths with spaces are
    // unrepresentable in that output, so splitting on whitespace is as good as it gets here.
    let includes: Vec<String> = php_config("--includes")?
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I").to_string())
        .collect();
    let prefix: String = php_config("--prefix")?;
    let version: (u32, u32) = parse_version(&php_config("--version")?)?;
    let bin: String = resolve_php_binary();
    let (zts, debug): (bool, bool) = detect_abi(&bin)?;
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

// Detect (zts, debug) from one `php -i`. ZTS reads the PHP_ZTS constant first (robust); its text
// fallback and the debug flag share the single `php -i` output.
#[cfg(unix)]
fn detect_abi(php_binary: &str) -> anyhow::Result<(bool, bool)> {
    let zts_const = Command::new(php_binary)
        .args(["-r", "echo PHP_ZTS;"])
        .output()
        .ok()
        .and_then(|out| match String::from_utf8_lossy(&out.stdout).trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None, // usually not possible, but fall back to `php -i` if it happens
        });

    let info = php_info(php_binary)?;
    let zts = match zts_const {
        Some(z) => z,
        None => php_info_field(&info, "Thread Safety")
            .map(|v| v == "enabled")
            .context("could not determine Thread Safety from `php -i`")?,
    };
    let debug = php_info_field(&info, "Debug Build") == Some("yes");
    Ok((zts, debug))
}

#[cfg(unix)]
fn php_info(php_binary: &str) -> anyhow::Result<String> {
    let out = Command::new(php_binary)
        .arg("-i")
        .output()
        .context("running `php -i`")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Value after a `Key => ` field in `php -i` output, trimmed.
#[cfg(unix)]
fn php_info_field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key} => ");
    info.lines()
        .find_map(|l| l.split(needle.as_str()).nth(1))
        .map(str::trim)
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
    let includes: Vec<String> = include_dirs
        .iter()
        .map(|d| inc.join(d).display().to_string())
        .collect();
    let lib_dir = root.join("lib");
    let version = parse_version(&read_php_version(&inc)?)?;

    // The devel pack's import libs are the ground truth for the ABI being linked (php8ts.lib /
    // php8ts_debug.lib / php8.lib / php8_debug.lib); whatever `php` happens to be first on PATH
    // may be a different install entirely.
    let major = version.0;
    let has_lib = |suffix: &str| lib_dir.join(format!("php{major}{suffix}.lib")).exists();
    let zts = has_lib("ts") || has_lib("ts_debug");
    let debug = has_lib("ts_debug") || has_lib("_debug");
    let lib_name = windows_lib_name(&lib_dir, major, zts, debug)?;
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

// ZTS ships php8ts.lib, NTS php8.lib, and a debug build may add a `_debug` suffix. Probe the
// candidates in preference order and link whichever the devel pack actually shipped.
#[cfg(windows)]
fn windows_lib_name(
    lib_dir: &std::path::Path,
    major: u32,
    zts: bool,
    debug: bool,
) -> anyhow::Result<String> {
    let base = if zts {
        format!("php{major}ts")
    } else {
        format!("php{major}")
    };
    let mut candidates = Vec::new();
    if debug {
        candidates.push(format!("{base}_debug"));
    }
    candidates.push(base);
    for stem in &candidates {
        if lib_dir.join(format!("{stem}.lib")).exists() {
            return Ok(stem.clone());
        }
    }
    anyhow::bail!(
        "no PHP import lib ({}) in {}",
        candidates
            .iter()
            .map(|s| format!("{s}.lib"))
            .collect::<Vec<_>>()
            .join(" / "),
        lib_dir.display()
    )
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
