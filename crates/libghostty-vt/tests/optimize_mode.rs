//! Guard against the issue-#59 regression: the VT parser runs on the
//! TUI's UI thread for every byte of agent output. Built in Zig's
//! default `Debug` mode it is ~570× slower and wedges the run loop —
//! the UI stalls and buffered keystrokes/events get dropped. The
//! `build.rs` for `libghostty-vt-sys` must pass `-Doptimize=ReleaseFast`;
//! this test pins that contract so a dropped flag fails CI instead of
//! shipping a frozen TUI.

use libghostty_vt::build_info::{OptimizeMode, optimize_mode};

#[test]
fn vt_parser_is_built_releasefast() {
    let mode = optimize_mode().expect("query libghostty optimize mode");
    assert_eq!(
        mode,
        OptimizeMode::ReleaseFast,
        "libghostty-vt must be built ReleaseFast (got {mode:?}); a Debug/ReleaseSafe \
         parser freezes the TUI on agent output — see libghostty-vt-sys/build.rs"
    );
}
