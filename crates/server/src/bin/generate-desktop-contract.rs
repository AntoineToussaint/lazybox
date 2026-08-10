#[cfg(not(feature = "desktop-contract"))]
compile_error!("run with --features desktop-contract");

use lazybox_server::api_gateway::{
    DESKTOP_PROTOCOL_FINGERPRINT, DESKTOP_PROTOCOL_VERSION, DESKTOP_TERMINAL_STREAM_ITEM_DATA,
    DESKTOP_TERMINAL_STREAM_ITEM_RESET, DesktopAgentInfo, DesktopAttentionSettings, DesktopCommand,
    DesktopDaemonSettings, DesktopEvent, DesktopInboxView, DesktopInfo, DesktopModelTier,
    DesktopRepository, DesktopStreamMessage, DesktopTerminalSnapshot, HealthResponse,
    ProtocolResponse, TERMINAL_CLIENT_COMMAND_CLOSE, TERMINAL_CLIENT_COMMAND_FETCH_SCROLLBACK,
    TERMINAL_CLIENT_COMMAND_RESIZE, TERMINAL_CLIENT_COMMAND_RESYNC, TERMINAL_CLIENT_COMMAND_WRITE,
    TERMINAL_CLIENT_FRAME_HEADER_BYTES, TERMINAL_CLIENT_FRAME_KIND_OFFSET,
    TERMINAL_CLIENT_FRAME_PAYLOAD_OFFSET, TERMINAL_CLIENT_FRAME_TERMINAL_ID_OFFSET,
    TERMINAL_FRAME_LENGTH_OFFSET, TERMINAL_FRAME_LENGTH_PREFIX_BYTES, TERMINAL_RESIZE_COLS_OFFSET,
    TERMINAL_RESIZE_PAYLOAD_BYTES, TERMINAL_RESIZE_ROWS_OFFSET, TERMINAL_RESYNC_PAYLOAD_BYTES,
    TERMINAL_RESYNC_REQUIRED_SEQ_OFFSET, TERMINAL_SERVER_FRAME_FIRST_SEQ_OFFSET,
    TERMINAL_SERVER_FRAME_HEADER_BYTES, TERMINAL_SERVER_FRAME_KIND_OFFSET,
    TERMINAL_SERVER_FRAME_LAST_SEQ_OFFSET, TERMINAL_SERVER_FRAME_OUTPUT,
    TERMINAL_SERVER_FRAME_PAYLOAD_OFFSET, TERMINAL_SERVER_FRAME_RESYNC,
    TERMINAL_SERVER_FRAME_RESYNC_UNAVAILABLE, TERMINAL_SERVER_FRAME_SCROLLBACK,
    TERMINAL_SERVER_FRAME_SNAPSHOT, TERMINAL_SERVER_FRAME_TERMINAL_ID_OFFSET,
    TERMINAL_WRITE_BYTES_OFFSET, TERMINAL_WRITE_INTENT_COMPOSE, TERMINAL_WRITE_INTENT_OFFSET,
    TERMINAL_WRITE_INTENT_SUBMIT, TERMINAL_WRITE_INTENT_VIEW, UnsupportedProtocolResponse,
    WorkspacesResponse,
};
use lazybox_tui_core::inbox::{Filter, FilterAxis, FilterMenuItem};
use lazybox_tui_core::snippets::{PickerRow, SnippetGroup, SnippetPickerView};
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

    HealthResponse::export_all(&config)?;
    ProtocolResponse::export_all(&config)?;
    UnsupportedProtocolResponse::export_all(&config)?;
    WorkspacesResponse::export_all(&config)?;
    DesktopCommand::export_all(&config)?;
    DesktopEvent::export_all(&config)?;
    DesktopTerminalSnapshot::export_all(&config)?;
    DesktopInfo::export_all(&config)?;
    DesktopAgentInfo::export_all(&config)?;
    DesktopModelTier::export_all(&config)?;
    DesktopDaemonSettings::export_all(&config)?;
    DesktopAttentionSettings::export_all(&config)?;
    DesktopRepository::export_all(&config)?;
    DesktopStreamMessage::export_all(&config)?;
    // The grouped inbox view-model (#732). `export_all` pulls in the
    // shared tui-core types it embeds: ComputeOutcome, VisibleRow,
    // WorkspaceKind, SortMode, RepoSummary.
    DesktopInboxView::export_all(&config)?;
    // Filter menu contract (#733): the desktop builds its filter menu
    // generically from these, never hardcoding the predicate list.
    FilterMenuItem::export_all(&config)?;
    Filter::export_all(&config)?;
    FilterAxis::export_all(&config)?;

    // Shared snippet-picker view-model (#734): grouped rows + auto-submit
    // computed by `tui-core::snippets`, so the desktop picker matches the
    // TUI's grouping/filter/recent/auto-submit.
    SnippetPickerView::export_all(&config)?;
    SnippetGroup::export_all(&config)?;
    PickerRow::export_all(&config)?;

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

    let index = "export type { DesktopCommand as LazyboxCommand } from \"./DesktopCommand\";\n\
         export type { DesktopEvent as LazyboxEvent } from \"./DesktopEvent\";\n\
         export type { DesktopInboxView } from \"./DesktopInboxView\";\n\
         export type { DesktopInfo } from \"./DesktopInfo\";\n\
         export type { DesktopRepository } from \"./DesktopRepository\";\n\
         export type { DesktopStreamMessage } from \"./DesktopStreamMessage\";\n\
         export type { ComputeOutcome } from \"./ComputeOutcome\";\n\
         export type { VisibleRow } from \"./VisibleRow\";\n\
         export type { WorkspaceKind } from \"./WorkspaceKind\";\n\
         export type { SnippetPickerView } from \"./SnippetPickerView\";\n\
         export type { SnippetGroup } from \"./SnippetGroup\";\n\
         export type { PickerRow } from \"./PickerRow\";\n\
         export type { Filter } from \"./Filter\";\n\
         export type { FilterAxis } from \"./FilterAxis\";\n\
         export type { FilterMenuItem } from \"./FilterMenuItem\";\n\
         export type { SortMode } from \"./SortMode\";\n\
         export type { Mailbox } from \"./Mailbox\";\n\
         export type { RepoSummary } from \"./RepoSummary\";\n\
         export type { TerminalKind } from \"./TerminalKind\";\n\
         export type { Activity } from \"./Activity\";\n\
         export type { ActivityFingerprint } from \"./ActivityFingerprint\";\n\
         export type { UserPrompt } from \"./UserPrompt\";\n\
         export type { DesktopTerminalSnapshot as TerminalSnapshot } from \"./DesktopTerminalSnapshot\";\n\
         export type { Task } from \"./Task\";\n\
         export type { Workspace } from \"./Workspace\";\n\
         export type { WorkspaceDiffTarget } from \"./WorkspaceDiffTarget\";\n\
         export type { WorkspaceDiffDto } from \"./WorkspaceDiffDto\";\n\
         export type { DiffFileDto } from \"./DiffFileDto\";\n\
         export type { DiffHunkDto } from \"./DiffHunkDto\";\n\
         export type { DiffLineDto } from \"./DiffLineDto\";\n\
         export type { DiffLineKindDto } from \"./DiffLineKindDto\";\n\
         export type { WorkspacesResponse } from \"./WorkspacesResponse\";\n\
         export { DESKTOP_PROTOCOL_FINGERPRINT, DESKTOP_PROTOCOL_VERSION } from \"./terminal-wire\";\n";
    std::fs::write(output.join("index.ts"), index)?;
    let terminal_wire = format!(
        "export const DESKTOP_PROTOCOL_VERSION = {DESKTOP_PROTOCOL_VERSION} as const;\n\
         export const DESKTOP_PROTOCOL_FINGERPRINT = {DESKTOP_PROTOCOL_FINGERPRINT} as const;\n\
         export const TERMINAL_SERVER_FRAME_HEADER_BYTES = {TERMINAL_SERVER_FRAME_HEADER_BYTES} as const;\n\
         export const TERMINAL_CLIENT_FRAME_HEADER_BYTES = {TERMINAL_CLIENT_FRAME_HEADER_BYTES} as const;\n\
         export const TERMINAL_SERVER_FRAME_LAYOUT = {{\n\
         \u{20}\u{20}lengthOffset: {TERMINAL_FRAME_LENGTH_OFFSET},\n\
         \u{20}\u{20}lengthPrefixBytes: {TERMINAL_FRAME_LENGTH_PREFIX_BYTES},\n\
         \u{20}\u{20}kindOffset: {TERMINAL_SERVER_FRAME_KIND_OFFSET},\n\
         \u{20}\u{20}terminalIdOffset: {TERMINAL_SERVER_FRAME_TERMINAL_ID_OFFSET},\n\
         \u{20}\u{20}firstSeqOffset: {TERMINAL_SERVER_FRAME_FIRST_SEQ_OFFSET},\n\
         \u{20}\u{20}lastSeqOffset: {TERMINAL_SERVER_FRAME_LAST_SEQ_OFFSET},\n\
         \u{20}\u{20}payloadOffset: {TERMINAL_SERVER_FRAME_PAYLOAD_OFFSET},\n\
         }} as const;\n\
         export const TERMINAL_CLIENT_FRAME_LAYOUT = {{\n\
         \u{20}\u{20}lengthOffset: {TERMINAL_FRAME_LENGTH_OFFSET},\n\
         \u{20}\u{20}lengthPrefixBytes: {TERMINAL_FRAME_LENGTH_PREFIX_BYTES},\n\
         \u{20}\u{20}kindOffset: {TERMINAL_CLIENT_FRAME_KIND_OFFSET},\n\
         \u{20}\u{20}terminalIdOffset: {TERMINAL_CLIENT_FRAME_TERMINAL_ID_OFFSET},\n\
         \u{20}\u{20}payloadOffset: {TERMINAL_CLIENT_FRAME_PAYLOAD_OFFSET},\n\
         }} as const;\n\
         export const TERMINAL_RESIZE_PAYLOAD_LAYOUT = {{\n\
         \u{20}\u{20}bytes: {TERMINAL_RESIZE_PAYLOAD_BYTES},\n\
         \u{20}\u{20}colsOffset: {TERMINAL_RESIZE_COLS_OFFSET},\n\
         \u{20}\u{20}rowsOffset: {TERMINAL_RESIZE_ROWS_OFFSET},\n\
         }} as const;\n\
         export const TERMINAL_RESYNC_PAYLOAD_LAYOUT = {{\n\
         \u{20}\u{20}bytes: {TERMINAL_RESYNC_PAYLOAD_BYTES},\n\
         \u{20}\u{20}requiredSeqOffset: {TERMINAL_RESYNC_REQUIRED_SEQ_OFFSET},\n\
         }} as const;\n\
         export const TERMINAL_WRITE_PAYLOAD_LAYOUT = {{\n\
         \u{20}\u{20}intentOffset: {TERMINAL_WRITE_INTENT_OFFSET},\n\
         \u{20}\u{20}bytesOffset: {TERMINAL_WRITE_BYTES_OFFSET},\n\
         }} as const;\n\
         export const TERMINAL_INPUT_INTENTS = {{\n\
         \u{20}\u{20}compose: {TERMINAL_WRITE_INTENT_COMPOSE},\n\
         \u{20}\u{20}submit: {TERMINAL_WRITE_INTENT_SUBMIT},\n\
         \u{20}\u{20}view: {TERMINAL_WRITE_INTENT_VIEW},\n\
         }} as const;\n\
         export const TERMINAL_SERVER_FRAME_KINDS = {{\n\
         \u{20}\u{20}snapshot: {TERMINAL_SERVER_FRAME_SNAPSHOT},\n\
         \u{20}\u{20}output: {TERMINAL_SERVER_FRAME_OUTPUT},\n\
         \u{20}\u{20}resync: {TERMINAL_SERVER_FRAME_RESYNC},\n\
         \u{20}\u{20}scrollback: {TERMINAL_SERVER_FRAME_SCROLLBACK},\n\
         \u{20}\u{20}resyncUnavailable: {TERMINAL_SERVER_FRAME_RESYNC_UNAVAILABLE},\n\
         }} as const;\n\
         export const TERMINAL_CLIENT_COMMAND_KINDS = {{\n\
         \u{20}\u{20}write: {TERMINAL_CLIENT_COMMAND_WRITE},\n\
         \u{20}\u{20}resize: {TERMINAL_CLIENT_COMMAND_RESIZE},\n\
         \u{20}\u{20}resync: {TERMINAL_CLIENT_COMMAND_RESYNC},\n\
         \u{20}\u{20}close: {TERMINAL_CLIENT_COMMAND_CLOSE},\n\
         \u{20}\u{20}fetchScrollback: {TERMINAL_CLIENT_COMMAND_FETCH_SCROLLBACK},\n\
         }} as const;\n\
         export const DESKTOP_TERMINAL_STREAM_ITEMS = {{\n\
         \u{20}\u{20}reset: {DESKTOP_TERMINAL_STREAM_ITEM_RESET},\n\
         \u{20}\u{20}data: {DESKTOP_TERMINAL_STREAM_ITEM_DATA},\n\
         }} as const;\n"
    );
    std::fs::write(output.join("terminal-wire.ts"), terminal_wire)?;
    Ok(())
}
