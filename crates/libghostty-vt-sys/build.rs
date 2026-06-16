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

    // Ghostty's build.zig pins `minimum_zig_version = "0.15.2"` AND
    // requires the same minor — newer zig (e.g. brew's default 0.16.x)
    // fails at comptime. Resolve the right zig binary in order:
    //
    //   1. `LAZYBOX_ZIG_BIN`     — explicit override, takes precedence.
    //   2. `zig` on PATH       — usually correct; fails fast if too new.
    //   3. Homebrew's `zig@0.15` keg path — best-effort fallback so a
    //      `brew install zig@0.15` (it's keg-only, doesn't link to
    //      /usr/local/bin) "just works" without the user editing PATH.
    //
    // The fallback only kicks in when the path exists, so non-mac CI
    // and users with a system zig 0.15 stay on the PATH binary.
    let zig_bin = resolve_zig_binary();
    println!("cargo:rerun-if-env-changed=LAZYBOX_ZIG_BIN");
    let mut build = Command::new(&zig_bin);
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        // ghostty's `build.zig` defaults `-Doptimize` to `.Debug`. The
        // VT parser runs on the TUI's UI thread for every byte of agent
        // output; a Debug build is ~570× slower and wedges the run loop
        // (buffered keystrokes age out, forward buffers overflow). Always
        // build the vendored parser ReleaseFast — we never debug it here.
        .arg("-Doptimize=ReleaseFast")
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

/// Pick the right `zig` binary to invoke. See the call site for the
/// resolution order. Returns either a LAZYBOX_ZIG_BIN override, the
/// system `zig` from PATH, or Homebrew's `zig@0.15` keg path on
/// macOS when system zig is missing or too new.
fn resolve_zig_binary() -> PathBuf {
    if let Ok(explicit) = env::var("LAZYBOX_ZIG_BIN") {
        return PathBuf::from(explicit);
    }

    // Probe the PATH zig's version. On any failure (missing,
    // non-zero exit, unexpected output) we fall through to the
    // platform fallback rather than try to interpret the error.
    let system_ver = Command::new("zig")
        .arg("version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());

    // Ghostty pins 0.15.x specifically; minor mismatch fails at
    // comptime. Treat anything that doesn't start with "0.15." as
    // wrong-minor.
    let system_is_compatible = system_ver
        .as_deref()
        .map(|v| v.starts_with("0.15."))
        .unwrap_or(false);
    if system_is_compatible {
        return PathBuf::from("zig");
    }

    // macOS Homebrew fallback. The `zig@0.15` formula is keg-only
    // so it won't appear on PATH after `brew install zig@0.15` —
    // probe the well-known location directly. Tried after the PATH
    // check so a user with a working zig 0.15 isn't second-guessed.
    let brew_paths = [
        "/opt/homebrew/opt/zig@0.15/bin/zig", // Apple Silicon
        "/usr/local/opt/zig@0.15/bin/zig",    // Intel Macs
    ];
    for candidate in brew_paths {
        if std::path::Path::new(candidate).exists() {
            eprintln!(
                "libghostty-vt-sys: system zig {} is incompatible with ghostty (needs 0.15.x); using brew zig@0.15 at {candidate}",
                system_ver.as_deref().unwrap_or("?"),
            );
            return PathBuf::from(candidate);
        }
    }

    // Nothing compatible found. Fall back to the PATH binary so
    // the user gets the ghostty `requireZig` error with the
    // actionable "install zig 0.15.x" hint, rather than our
    // version-shim error.
    PathBuf::from("zig")
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

// Note: appended by lazybox for static linking support
