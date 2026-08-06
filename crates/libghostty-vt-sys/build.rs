use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT_RAW: &str = include_str!("../../.ghostty-version");
const ZIG_VERSION_RAW: &str = include_str!("../../.zig-version");

fn ghostty_commit() -> &'static str {
    GHOSTTY_COMMIT_RAW.trim()
}

fn zig_version() -> &'static str {
    ZIG_VERSION_RAW.trim()
}

/// The `major.minor` series of the pinned zig (e.g. `0.16` for `0.16.0`).
/// Ghostty's `requireZig` accepts only the matching minor, and Homebrew
/// names its keg-only formulae `zig@<major.minor>`.
fn zig_minor_series() -> String {
    let v = zig_version();
    v.split('.').take(2).collect::<Vec<_>>().join(".")
}

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_NO_VENDOR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=LAZYBOX_GHOSTTY_CACHE");
    println!("cargo:rerun-if-env-changed=LAZYBOX_ZIG_CACHE");
    println!("cargo:rerun-if-env-changed=LAZYBOX_ZIG_GLOBAL_CACHE");
    println!("cargo:rerun-if-env-changed=LAZYBOX_OFFLINE");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    // rerun-if-changed paths are resolved relative to this package's root
    // (crates/libghostty-vt-sys/), not the workspace root. build.rs sits at
    // that root, so watch it by bare name — a workspace-relative path here
    // points at a nonexistent file, which cargo treats as perpetually stale
    // and reruns the script (and the whole downstream chain) every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.ghostty-version");
    println!("cargo:rerun-if-changed=../../.zig-version");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let target = env::var("TARGET").expect("TARGET must be set");
    let host = env::var("HOST").expect("HOST must be set");

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

    // Ghostty's build.zig pins a `minimum_zig_version` AND requires the
    // same minor, so a zig from a different minor fails at comptime. The
    // matching pin is in `.zig-version` (the single source of truth shared
    // with the Makefile, bootstrap.sh, and CI). Resolve the right zig
    // binary in order:
    //
    //   1. `LAZYBOX_ZIG_BIN` — explicit override, takes precedence.
    //   2. The worktree/shared cache populated by `make setup`.
    //   3. A compatible `zig` on PATH.
    //   4. Homebrew's versioned keg path — best-effort fallback so a
    //      `brew install zig@<minor>` (it's keg-only, doesn't link to
    //      /usr/local/bin) "just works" without the user editing PATH.
    //
    // The fallback only kicks in when the path exists, so non-mac CI
    // and users with a matching system zig stay on the PATH binary.
    let zig_bin = resolve_zig_binary(&host);
    println!("cargo:rerun-if-env-changed=LAZYBOX_ZIG_BIN");
    let mut build = Command::new(&zig_bin);
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        // Zig defaults to the Debug optimize mode, which leaves the VT
        // stream parser ~1000x slower than an optimized build (~0.03 MB/s
        // measured). The TUI feeds PTY output through `vt_write` on its UI
        // thread, so a debug parser turns any agent output burst into a
        // multi-second freeze. ReleaseSafe (not ReleaseFast) keeps Zig's
        // runtime safety checks — this parser consumes untrusted terminal
        // output, where bounds/overflow checks guard against UB on
        // malformed escape sequences — while still optimizing.
        .arg("-Doptimize=ReleaseSafe")
        .arg("--prefix")
        .arg(&install_prefix)
        // The source checkout is shared between profiles and worktrees.
        // Keep Zig's mutable local cache profile-specific so concurrent
        // builds never write into or lock the shared source tree.
        .arg("--cache-dir")
        .arg(out_dir.join("ghostty-zig-cache"))
        // Keep Ghostty's downloaded Zig packages in the same persistent
        // cache family as its source. `make setup` warms this directory,
        // after which `make release` can run without network access.
        .arg("--global-cache-dir")
        .arg(zig_global_cache_dir(&out_dir))
        .current_dir(&ghostty_dir);

    // Always pass -Dtarget explicitly to avoid xcframework generation
    // on macOS (which requires full Xcode.app, not just Command Line Tools).
    let zig_target = zig_target(&target);
    build.arg(format!("-Dtarget={zig_target}"));

    // macOS 26 (Tahoe) SDK workaround. The pinned zig's bundled LLD can't follow
    // the Tahoe `libSystem.tbd` reexport chain when linking in native mode,
    // so every libc symbol (_free, _malloc, _waitpid, _dispatch_*, …) comes
    // back undefined — this kills even zig's internal build *runner* before
    // ghostty compiles at all, and ghostty's `apple_sdk` deliberately forces
    // the native SDK into the link, so there's no env-only escape. If native
    // linking is broken on this host AND an older, linkable macOS SDK is
    // installed, route the zig subprocess at it via an `xcrun --show-sdk-path`
    // shim on PATH. No-op (returns None) on machines and CI where native
    // linking already works, so nothing changes off Tahoe.
    if let Some(shim_dir) = macos_sdk_shim(&zig_bin, &out_dir) {
        let existing = env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![shim_dir];
        parts.extend(env::split_paths(&existing));
        let joined = env::join_paths(parts).expect("join PATH for zig SDK shim");
        build.env("PATH", joined);
    }

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");

    let lib_name = if target.contains("darwin") {
        "libghostty-vt.0.1.0.dylib"
    } else {
        "libghostty-vt.so.0.1.0"
    };

    // Run zig build. On macOS without Xcode.app, the xcframework step fails
    // but the actual library + headers are still produced. Check outputs
    // instead of panicking on exit code.
    //
    // On a cold cache `zig build` downloads Ghostty's ~15 package
    // dependencies from deps.files.ghostty.org before it compiles anything.
    // That network step has no internal retry, so — like `fetch_ghostty`'s
    // git clone before this fix — a single transient stream cancellation
    // used to abort the whole build with exit 101, and a plain rebuild then
    // succeeded. Retry a few times: zig persists already-fetched packages in
    // the global cache and compiled objects in the local cache, so a retry
    // resumes from where it left off rather than rebuilding from scratch. A
    // genuinely deterministic failure just exhausts the retries and falls
    // through to the same diagnostic below.
    const MAX_ZIG_ATTEMPTS: u32 = 3;
    let outputs_present =
        || lib_dir.join(lib_name).exists() && include_dir.join("ghostty").join("vt.h").exists();
    let mut status = build
        .status()
        .unwrap_or_else(|error| panic!("failed to execute zig build: {error}"));
    for attempt in 2..=MAX_ZIG_ATTEMPTS {
        if status.success() || outputs_present() {
            break;
        }
        eprintln!(
            "libghostty-vt: zig build failed ({status}); retrying (attempt {attempt}/{MAX_ZIG_ATTEMPTS}) ..."
        );
        status = build
            .status()
            .unwrap_or_else(|error| panic!("failed to execute zig build: {error}"));
    }

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
        // The optimized archive inlines libc++'s `__throw_*` paths into a
        // `__libcpp_verbose_abort` call (the Debug build didn't), a symbol
        // ubuntu-22.04's LLVM 14 libc++ predates. ghostty doesn't bundle
        // libc++, so supply a weak fallback. Emitted AFTER the ghostty
        // link-lib so single-pass linkers still see the pending undefined
        // symbol when this archive is processed.
        link_libcpp_verbose_abort_shim();
    }
    println!("cargo:include={}", include_dir.display());
}

/// Compile and link the weak `__libcpp_verbose_abort` fallback (see
/// `libcpp_shim.c`). `cc::Build::compile` emits the `rustc-link-lib=static`
/// + search-path directives, which propagate to the final binary link —
/// unlike `rustc-link-arg`, which wouldn't from a library crate. Linux
/// only; the caller gates on target.
fn link_libcpp_verbose_abort_shim() {
    let shim =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"))
            .join("libcpp_shim.c");
    println!("cargo:rerun-if-changed={}", shim.display());
    cc::Build::new().file(&shim).compile("ghostty_libcpp_shim");
}

/// Pick the right `zig` binary to invoke. See the call site for the
/// resolution order. Returns either a LAZYBOX_ZIG_BIN override, the exact
/// project-pinned binary prepared by `make setup`, a compatible system Zig,
/// or Homebrew's versioned keg path on macOS.
fn resolve_zig_binary(host: &str) -> PathBuf {
    if let Ok(explicit) = env::var("LAZYBOX_ZIG_BIN") {
        return PathBuf::from(explicit);
    }

    let host_slug = match host {
        "aarch64-apple-darwin" => Some("aarch64-macos"),
        "x86_64-apple-darwin" => Some("x86_64-macos"),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => Some("aarch64-linux"),
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => Some("x86_64-linux"),
        _ => None,
    };
    if let Some(host_slug) = host_slug {
        let manifest = PathBuf::from(
            env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
        );
        let workspace = manifest
            .parent()
            .and_then(Path::parent)
            .expect("libghostty-vt-sys must live under the workspace crates directory");
        let relative = format!("{host_slug}-{}/zig", zig_version());
        let local = workspace.join("vendor/zig").join(&relative);
        if local.is_file() {
            return local;
        }

        let cache_root = env::var_os("LAZYBOX_ZIG_CACHE")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".cache/lazybox/zig"))
            });
        if let Some(cache_root) = cache_root {
            let cached = cache_root.join(relative);
            if cached.is_file() {
                return cached;
            }
        }
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

    // Ghostty pins one minor specifically; a mismatch fails at comptime.
    // Derive the accepted prefix from `.zig-version` rather than hardcoding
    // it, so a pin bump can't leave this accepting (or worse, actively
    // selecting) the previous minor.
    let pinned_minor = zig_minor_series();
    let system_is_compatible = system_ver
        .as_deref()
        .map(|v| v.starts_with(&format!("{pinned_minor}.")))
        .unwrap_or(false);
    if system_is_compatible {
        return PathBuf::from("zig");
    }

    // macOS Homebrew fallback. The versioned formula is keg-only so it
    // won't appear on PATH after `brew install zig@<minor>` — probe the
    // well-known location directly. Tried after the PATH check so a user
    // with a working pinned zig isn't second-guessed.
    let brew_paths = [
        format!("/opt/homebrew/opt/zig@{pinned_minor}/bin/zig"), // Apple Silicon
        format!("/usr/local/opt/zig@{pinned_minor}/bin/zig"),    // Intel Macs
    ];
    for candidate in &brew_paths {
        if std::path::Path::new(candidate).exists() {
            eprintln!(
                "libghostty-vt-sys: system zig {} is incompatible with ghostty \
                 (needs {pinned_minor}.x); using brew zig@{pinned_minor} at {candidate}",
                system_ver.as_deref().unwrap_or("?"),
            );
            return PathBuf::from(candidate);
        }
    }

    panic!(
        "Zig {} is required but was not found. Run `make setup` once while online, then retry.",
        zig_version()
    )
}

/// Return the persistent Zig package cache used by Ghostty builds.
fn zig_global_cache_dir(out_dir: &Path) -> PathBuf {
    let path = env::var_os("LAZYBOX_ZIG_GLOBAL_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| ghostty_cache_root(out_dir).join("zig-global-cache"));
    std::fs::create_dir_all(&path)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", path.display()));
    path
}

/// Resolve the host-level Ghostty cache. Keeping it outside `target/` means
/// `cargo clean`, profile switches, and separate worktrees all reuse one
/// pinned checkout instead of cloning hundreds of megabytes each time.
fn ghostty_cache_root(out_dir: &Path) -> PathBuf {
    if let Some(path) = env::var_os("LAZYBOX_GHOSTTY_CACHE") {
        return PathBuf::from(path);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/lazybox/ghostty");
    }
    println!("cargo:warning=HOME is not set; Ghostty source cache will live under Cargo OUT_DIR");
    out_dir.join("ghostty-cache")
}

fn cached_source_is_valid(src_dir: &Path) -> bool {
    let stamp = src_dir.join(".ghostty-commit");
    src_dir.join("build.zig").is_file()
        && std::fs::read_to_string(stamp)
            .map(|value| value.trim() == ghostty_commit())
            .unwrap_or(false)
}

fn offline_requested() -> bool {
    env::var("LAZYBOX_OFFLINE")
        .map(|value| !matches!(value.as_str(), "" | "0" | "false" | "no"))
        .unwrap_or(false)
}

/// Clone Ghostty once into a persistent, commit-addressed cache. A temporary
/// checkout plus atomic rename makes concurrent first builds safe: the first
/// completed clone wins and the others discard their temporary copies.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let cache_root = ghostty_cache_root(out_dir);
    std::fs::create_dir_all(&cache_root)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", cache_root.display()));
    let src_dir = cache_root.join(format!("src-{}", ghostty_commit()));

    if cached_source_is_valid(&src_dir) {
        return src_dir;
    }

    if offline_requested() {
        panic!(
            "Ghostty source {} is not available in the offline cache at {}.\n\
             Run `make setup` once while online, then retry `make release`.",
            ghostty_commit(),
            cache_root.display()
        );
    }

    let temp_dir = cache_root.join(format!(
        ".src-{}.tmp-{}",
        ghostty_commit(),
        std::process::id()
    ));

    // Up to 3 attempts. Transient HTTP/2 stream cancellations from
    // GitHub mid-clone are surprisingly common on flaky networks
    // and used to brick fresh builds with a single panic. Each
    // retry cleans the partial src_dir first so the next clone
    // starts from a known-empty target.
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_error: Option<String> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", temp_dir.display()));
        }
        if attempt > 1 {
            eprintln!(
                "Fetching ghostty {} (attempt {attempt}/{MAX_ATTEMPTS}) ...",
                ghostty_commit()
            );
        } else {
            eprintln!("Fetching ghostty {} ...", ghostty_commit());
        }
        let mut clone = Command::new("git");
        clone
            .arg("clone")
            .arg("--filter=blob:none")
            .arg("--no-checkout")
            .arg(GHOSTTY_REPO)
            .arg(&temp_dir);
        if let Err(e) = try_run(clone, "git clone ghostty") {
            last_error = Some(e);
            continue;
        }
        let mut checkout = Command::new("git");
        checkout
            .arg("checkout")
            .arg(ghostty_commit())
            .current_dir(&temp_dir);
        if let Err(e) = try_run(checkout, "git checkout ghostty commit") {
            last_error = Some(e);
            continue;
        }
        std::fs::write(temp_dir.join(".ghostty-commit"), ghostty_commit())
            .unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

        if cached_source_is_valid(&src_dir) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return src_dir;
        }
        if src_dir.exists() {
            std::fs::remove_dir_all(&src_dir)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", src_dir.display()));
        }
        std::fs::rename(&temp_dir, &src_dir).unwrap_or_else(|error| {
            if cached_source_is_valid(&src_dir) {
                let _ = std::fs::remove_dir_all(&temp_dir);
            } else {
                panic!(
                    "failed to install Ghostty cache {}: {error}",
                    src_dir.display()
                );
            }
        });
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
             cd /tmp/ghostty-src && git checkout {}\n\
             GHOSTTY_SOURCE_DIR=/tmp/ghostty-src cargo build\n\
         - Check network connectivity to github.com.",
        ghostty_commit(),
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

/// Detect the macOS 26 (Tahoe) libSystem link breakage and, if present,
/// return a directory holding an `xcrun` shim that redirects
/// `--show-sdk-path` at an older, linkable SDK. Prepend it to the zig
/// subprocess's PATH. See the call site for the full rationale. Returns
/// `None` when native linking already works or no workaround is possible.
#[cfg(not(target_os = "macos"))]
fn macos_sdk_shim(_zig_bin: &Path, _out_dir: &Path) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn macos_sdk_shim(zig_bin: &Path, out_dir: &Path) -> Option<PathBuf> {
    let work = out_dir.join("sdk-probe");
    std::fs::create_dir_all(&work).ok()?;

    // 1. Does native libc linking work as-is? Most machines — and CI on
    //    older macOS — say yes; bail out for zero behavior change.
    if zig_links_natively(zig_bin, &work, None) {
        return None;
    }

    // 2. Native link is broken (Tahoe). Probe installed SDKs newest-first so
    //    we stay as close to the host as possible, and pick the first that
    //    the pinned zig can actually link against. The unversioned
    //    `MacOSX.sdk` (the broken newest) is skipped during discovery.
    let mut sdks = discover_macos_sdks();
    sdks.sort_by(|a, b| b.1.cmp(&a.1));
    for (sdk, _ver) in &sdks {
        if zig_links_natively(zig_bin, &work, Some(sdk)) {
            let shim_dir = out_dir.join("sdk-shim");
            write_xcrun_shim(&shim_dir, sdk)?;
            println!(
                "cargo:warning=libghostty-vt: this macOS SDK won't link with the pinned zig \
                 (Tahoe libSystem reexport chain); routing zig at {} instead",
                sdk.display()
            );
            return Some(shim_dir);
        }
    }

    // 3. Nothing linkable. Let the build proceed to zig's own (clearer)
    //    error, but leave an actionable breadcrumb first.
    println!(
        "cargo:warning=libghostty-vt: native macOS SDK won't link with the pinned zig and no older \
         linkable SDK was found. Install an older Command Line Tools SDK (e.g. MacOSX15.sdk) or \
         use a prebuilt lazybox release."
    );
    None
}

/// Try to link a minimal libc-backed zig program with the given SDK routed
/// in via a throwaway shim (or the native SDK when `sdk` is `None`). Returns
/// true only on a clean link — the output file is left behind even on
/// failure, so the exit status is the only reliable signal.
#[cfg(target_os = "macos")]
fn zig_links_natively(zig_bin: &Path, work: &Path, sdk: Option<&Path>) -> bool {
    // Pull in the C allocator + posix wrappers — the same libSystem surface
    // that fails against a `libSystem.tbd` zig's LLD can't parse.
    let src = work.join("probe.zig");
    let program = "const std = @import(\"std\");\n\
         pub fn main() !void {\n\
         \x20   const a = std.heap.c_allocator;\n\
         \x20   const p = a.alloc(u8, 8) catch return;\n\
         \x20   a.free(p);\n\
         }\n";
    if std::fs::write(&src, program).is_err() {
        return false;
    }

    let mut cmd = Command::new(zig_bin);
    cmd.arg("build-exe")
        .arg(&src)
        .arg("-lc")
        .arg(format!("-femit-bin={}", work.join("probe-bin").display()))
        .current_dir(work)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(sdk) = sdk {
        let shim = work.join("probe-shim");
        if write_xcrun_shim(&shim, sdk).is_none() {
            return false;
        }
        let existing = env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![shim];
        parts.extend(env::split_paths(&existing));
        if let Ok(joined) = env::join_paths(parts) {
            cmd.env("PATH", joined);
        }
    }

    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Write an executable `xcrun` shim into `dir` that answers
/// `--show-sdk-path` with `sdk` and passes every other invocation through to
/// the real `/usr/bin/xcrun` (so `xcode-select` and other queries still
/// work). Returns `Some(())` on success.
#[cfg(target_os = "macos")]
fn write_xcrun_shim(dir: &Path, sdk: &Path) -> Option<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).ok()?;
    let script = dir.join("xcrun");
    let body = format!(
        "#!/bin/sh\n\
         for a in \"$@\"; do\n\
         \x20 if [ \"$a\" = \"--show-sdk-path\" ]; then echo '{}'; exit 0; fi\n\
         done\n\
         exec /usr/bin/xcrun \"$@\"\n",
        sdk.display()
    );
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(())
}

/// Enumerate installed macOS SDKs (Command Line Tools + any selected
/// Xcode.app), returning `(canonical_path, (major, minor))`. The
/// unversioned `MacOSX.sdk` symlink is skipped — it points at the newest
/// SDK, which is exactly the one that won't link. Canonicalization dedupes
/// the `MacOSX26.sdk -> MacOSX26.5.sdk` version symlinks.
#[cfg(target_os = "macos")]
fn discover_macos_sdks() -> Vec<(PathBuf, (u32, u32))> {
    let mut roots = vec![PathBuf::from("/Library/Developer/CommandLineTools/SDKs")];
    if let Some(dev) = Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        roots.push(PathBuf::from(dev).join("Platforms/MacOSX.platform/Developer/SDKs"));
    }

    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(ver_str) = name
                .strip_prefix("MacOSX")
                .and_then(|s| s.strip_suffix(".sdk"))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Some(ver) = parse_macos_version(ver_str) else {
                continue;
            };
            let path = entry.path();
            let canon = std::fs::canonicalize(&path).unwrap_or(path);
            if seen.insert(canon.clone()) {
                found.push((canon, ver));
            }
        }
    }
    found
}

/// Parse a `<major>[.<minor>]` SDK version string (e.g. "15.4", "26").
#[cfg(target_os = "macos")]
fn parse_macos_version(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split('.');
    let major = it.next()?.parse::<u32>().ok()?;
    let minor = it.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
    Some((major, minor))
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
