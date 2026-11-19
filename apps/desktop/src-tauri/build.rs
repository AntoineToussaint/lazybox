use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=src");
    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("build clock must be after the Unix epoch")
        .as_secs();
    println!("cargo:rustc-env=LAZYBOX_DESKTOP_BUILT_AT={built_at}");
    tauri_build::build();
}
