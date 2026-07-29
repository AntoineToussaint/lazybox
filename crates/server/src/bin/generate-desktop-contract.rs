#[cfg(not(feature = "desktop-contract"))]
compile_error!("run with --features desktop-contract");

use lazybox_ipc::{Command, Event};
use lazybox_server::api_gateway::{
    CommandResponse, DESKTOP_PROTOCOL_VERSION, DesktopInfo, DesktopStreamMessage, HealthResponse,
    JsonClientFrame, JsonServerFrame, ProtocolResponse, UnsupportedProtocolResponse,
    WorkspacesResponse,
};
use std::path::PathBuf;
use ts_rs::{Config, TS};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/desktop/src/generated");
    std::fs::create_dir_all(&output)?;
    for entry in std::fs::read_dir(&output)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "ts") {
            std::fs::remove_file(path)?;
        }
    }
    let config = Config::from_env()
        .with_out_dir(&output)
        .with_large_int("number");

    Command::export_all(&config)?;
    Event::export_all(&config)?;
    HealthResponse::export_all(&config)?;
    ProtocolResponse::export_all(&config)?;
    UnsupportedProtocolResponse::export_all(&config)?;
    WorkspacesResponse::export_all(&config)?;
    CommandResponse::export_all(&config)?;
    JsonClientFrame::export_all(&config)?;
    JsonServerFrame::export_all(&config)?;
    DesktopInfo::export_all(&config)?;
    DesktopStreamMessage::export_all(&config)?;

    for entry in std::fs::read_dir(&output)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "ts") {
            let source = std::fs::read_to_string(&path)?;
            let normalized = source
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(path, normalized)?;
        }
    }

    let index = format!(
        "export type {{ Command as LazyboxCommand }} from \"./Command\";\n\
         export type {{ Event as LazyboxEvent }} from \"./Event\";\n\
         export type {{ DesktopInfo }} from \"./DesktopInfo\";\n\
         export type {{ DesktopStreamMessage }} from \"./DesktopStreamMessage\";\n\
         export type {{ TerminalKind }} from \"./TerminalKind\";\n\
         export type {{ TerminalSnapshot }} from \"./TerminalSnapshot\";\n\
         export type {{ Task }} from \"./Task\";\n\
         export type {{ Workspace }} from \"./Workspace\";\n\
         export type {{ WorkspacesResponse }} from \"./WorkspacesResponse\";\n\
         export const DESKTOP_PROTOCOL_VERSION = {DESKTOP_PROTOCOL_VERSION} as const;\n"
    );
    std::fs::write(output.join("index.ts"), index)?;
    Ok(())
}
