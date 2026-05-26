use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "a1e75daef8b64426dbca551c6e41b1fbc2b7ae24";

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_NO_VENDOR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    // rerun-if-changed paths are resolved relative to this package's root
    // (crates/libghostty-vt-sys/), not the workspace root. build.rs sits at
    // that root, so watch it by bare name — a workspace-relative path here
    // points at a nonexistent file, which cargo treats as perpetually stale
    // and reruns the script (and the whole downstream chain) every build.
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let target = env::var("TARGET").expect("TARGET must be set");
    let _host = env::var("HOST").expect("HOST must be set");

    // Locate ghostty source: env override > fetch into OUT_DIR.
    let ghostty_dir = match env::var("GHOSTTY_SOURCE_DIR") {
        Ok(dir) => {
            let p = PathBuf::from(dir);
            assert!(
                p.join("build.zig").exists(),
                "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
                p.display()
            );
            p
        }
        Err(_) => fetch_ghostty(&out_dir),
    };

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg("--prefix")
        .arg(&install_prefix)
        .current_dir(&ghostty_dir);

    // Always pass -Dtarget explicitly to avoid xcframework generation
    // on macOS (which requires full Xcode.app, not just Command Line Tools).
    let zig_target = zig_target(&target);
    build.arg(format!("-Dtarget={zig_target}"));

    // Run zig build. On macOS without Xcode.app, the xcframework step fails
    // but the actual library + headers are still produced. Check outputs
    // instead of panicking on exit code.
    let status = build
        .status()
        .unwrap_or_else(|error| panic!("failed to execute zig build: {error}"));

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");

    let lib_name = if target.contains("darwin") {
        "libghostty-vt.0.1.0.dylib"
    } else {
        "libghostty-vt.so.0.1.0"
    };

    if !status.success() {
        // Check if outputs exist despite build failure (xcframework issue).
        if lib_dir.join(lib_name).exists() && include_dir.join("ghostty").join("vt.h").exists() {
            eprintln!("cargo:warning=zig build exited with {status} but outputs exist, continuing");
        } else {
            panic!("zig build failed with status {status} and outputs are missing");
        }
    } else {
        assert!(
            lib_dir.join(lib_name).exists(),
            "expected shared library at {}",
            lib_dir.join(lib_name).display()
        );
        assert!(
            include_dir.join("ghostty").join("vt.h").exists(),
            "expected header at {}",
            include_dir.join("ghostty").join("vt.h").display()
        );
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    if target.contains("darwin") {
        println!("cargo:rustc-link-lib=framework=System");
        println!("cargo:rustc-link-lib=c");
        println!("cargo:rustc-link-lib=c++");
    } else if target.contains("linux") {
        // Zig builds against libc++ (LLVM's C++ stdlib) by default,
        // so the vendored ghostty archive references `std::__1::*`
        // symbols (simdutf, highway, std::optional) which only libc++
        // provides — libstdc++ uses `std::__cxx11::*`. Link both
        // libc++ AND libc++abi (typeinfo + RTTI live in libc++abi),
        // plus libc for sanity. CI installs `libc++-dev libc++abi-dev`.
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-link-lib=c++abi");
        println!("cargo:rustc-link-lib=c");
    }
    println!("cargo:include={}", include_dir.display());
}

/// Clone ghostty at the pinned commit into OUT_DIR/ghostty-src.
/// Reuses an existing clone if the commit matches.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let src_dir = out_dir.join("ghostty-src");
    let stamp = src_dir.join(".ghostty-commit");

    // Skip fetch if we already have the right commit.
    if stamp.exists()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing.trim() == GHOSTTY_COMMIT
    {
        return src_dir;
    }

    // Up to 3 attempts. Transient HTTP/2 stream cancellations from
    // GitHub mid-clone are surprisingly common on flaky networks
    // and used to brick fresh builds with a single panic. Each
    // retry cleans the partial src_dir first so the next clone
    // starts from a known-empty target.
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_error: Option<String> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        if src_dir.exists() {
            std::fs::remove_dir_all(&src_dir)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", src_dir.display()));
        }
        if attempt > 1 {
            eprintln!("Fetching ghostty {GHOSTTY_COMMIT} (attempt {attempt}/{MAX_ATTEMPTS}) ...");
        } else {
            eprintln!("Fetching ghostty {GHOSTTY_COMMIT} ...");
        }
        let mut clone = Command::new("git");
        clone
            .arg("clone")
            .arg("--filter=blob:none")
            .arg("--no-checkout")
            .arg(GHOSTTY_REPO)
            .arg(&src_dir);
        if let Err(e) = try_run(clone, "git clone ghostty") {
            last_error = Some(e);
            continue;
        }
        let mut checkout = Command::new("git");
        checkout
            .arg("checkout")
            .arg(GHOSTTY_COMMIT)
            .current_dir(&src_dir);
        if let Err(e) = try_run(checkout, "git checkout ghostty commit") {
            last_error = Some(e);
            continue;
        }
        std::fs::write(&stamp, GHOSTTY_COMMIT)
            .unwrap_or_else(|e| panic!("failed to write stamp: {e}"));
        return src_dir;
    }
    let last = last_error.unwrap_or_else(|| "unknown error".to_string());
    panic!(
        "Failed to fetch ghostty source after {MAX_ATTEMPTS} attempts: {last}\n\
         \n\
         Workarounds:\n\
         - Re-run the build; the next attempt usually succeeds.\n\
         - Clone ghostty manually then set GHOSTTY_SOURCE_DIR:\n\
             git clone --filter=blob:none {GHOSTTY_REPO} /tmp/ghostty-src\n\
             cd /tmp/ghostty-src && git checkout {GHOSTTY_COMMIT}\n\
             GHOSTTY_SOURCE_DIR=/tmp/ghostty-src cargo build\n\
         - Check network connectivity to github.com.",
    );
}

/// Run a command, returning its non-zero exit / spawn error as a
/// string instead of panicking. Used by the fetch retry loop.
fn try_run(mut command: Command, context: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("failed to execute {context}: {e}"))?;
    if !status.success() {
        return Err(format!("{context} failed with status {status}"));
    }
    Ok(())
}

#[allow(dead_code)]
fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}

// Note: appended by pilot for static linking support
