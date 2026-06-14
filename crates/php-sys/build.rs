use std::process::Output;

fn main() {
    let php_includes = php_config("--includes");
    let php_prefix = php_config("--prefix");
    let php_version = php_config("--version");

    // where is to search libs
    println!("cargo:rustc-link-search=native={}/lib", php_prefix.trim());
    println!("cargo:rustc-link-lib=dylib=php");

    for (major, minor) in [(8, 3), (8, 4), (8, 5)] {
        if (major, minor) >= (major, minor) {
            println!("cargo:rustc-cfg=php{major}{minor}");
        }
    }
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
