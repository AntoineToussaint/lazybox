//! Guards against the vendored ghostty VT library being compiled in
//! Zig's Debug optimize mode.
//!
//! Debug mode leaves `vt_write` ~570x slower than an optimized build
//! (~0.03 MB/s vs ~17 MB/s measured). Because the TUI feeds PTY output
//! through `vt_write` on its UI thread, a debug parser turns any agent
//! output burst into a multi-second freeze — the symptom behind the
//! "pressing `w` makes the app unresponsive" bug. `libghostty-vt-sys`'s
//! build script passes `-Doptimize=ReleaseSafe`; this test fails loudly
//! if that flag is ever dropped.

use libghostty_vt::build_info::{OptimizeMode, optimize_mode};

#[test]
fn library_is_not_built_in_debug_mode() {
    let mode = optimize_mode().expect("query optimize mode");
    assert_ne!(
        mode,
        OptimizeMode::Debug,
        "libghostty-vt was built in Debug mode — vt_write is ~570x slower, \
         which freezes the TUI under agent output. Ensure \
         libghostty-vt-sys/build.rs passes -Doptimize=ReleaseSafe.",
    );
}
