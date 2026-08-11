use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo manifest"));
    let repository = manifest.join("../../..");
    let output = Command::new("git")
        .args([
            "-C",
            repository.to_str().expect("utf-8 path"),
            "rev-parse",
            "--short=12",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LAZYBOX_DESKTOP_BUILD_SHA={output}");
    println!(
        "cargo:rerun-if-changed={}",
        repository.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repository.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("../package.json").display()
    );
    tauri_build::build()
}
