#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Output;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let php_includes = php_config("--includes");
    let php_prefix = php_config("--prefix");
    let php_version = php_config("--version");

    // should be 8.5.7 -> 8.5
    let mut ver = php_version.trim().split(".");
    let bad = || format!("unsupported PHP version {php_version:?}");
    // next -> 8
    let major: u32 = ver.next().ok_or_else(bad)?.parse()?;
    // next -> 5
    let minor: u32 = ver.next().ok_or_else(bad)?.parse()?;

    // where is to search libs
    println!("cargo:rustc-link-search=native={}/lib", php_prefix.trim());
    println!("cargo:rustc-link-lib=dylib=php");

    for (vmajor, vminor) in [(8, 2), (8, 3), (8, 4), (8, 5)] {
        if (major, minor) >= (vmajor, vminor) {
            println!("cargo:rustc-cfg=php{vmajor}{vminor}");
        }
    }

    // compile c code
    let includes: Vec<&str> = php_includes
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I"))
        .collect();

    let mut c = cc::Build::new();
    c.file("wrapper.c").file("module.c").define("ZTS", None);

    for d in &includes {
        c.include(d);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(php_includes.split_whitespace())
        .clang_arg("-DZTS")
        .layout_tests(true); // size/offset asserts

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

    for f in ["wrapper.h", "module.c", "wrapper.c"] {
        println!("cargo:rerun-if-changed={f}");
    }

    Ok(())
}

fn php_config(arg: &str) -> String {
    let output: Output = std::process::Command::new("php-config")
        .arg(arg)
        .output()
        .expect("Failed to execute php-config");

    if !output.status.success() {
        panic!(
            "php-config failed with status: {}",
            output.status.code().unwrap_or(-1)
        );
    }

    String::from_utf8(output.stdout).expect("Invalid UTF-8 output from php-config")
}
