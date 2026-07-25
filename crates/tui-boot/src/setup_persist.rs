//! Store-backed persistence for the setup wizard's answers.
//!
//! The pure YAML core (`load_from_yaml` / `save_persisted_yaml` /
//! `config_yaml_path`) lives in the UI library (`lazybox_tui::setup_flow`);
//! `~/.lazybox/config.yaml` is the source of truth. These wrappers add the
//! legacy SQLite `KV_KEY_SETUP` blob handling — a load-time fallback and a
//! save-time cleanup — which needs a store handle, so they live boot-side
//! where the store lives (#548).

use lazybox_core::{KV_KEY_SETUP, PersistedSetup};
use lazybox_store::Store;
use lazybox_tui::setup_flow::{config_yaml_path, load_from_yaml, save_persisted_yaml};

/// Load the persisted setup from `~/.lazybox/config.yaml::setup`.
///
/// Migration: an older release wrote setup state to the SQLite kv blob
/// (`KV_KEY_SETUP`). We still check that as a fallback so existing users
/// don't lose their wizard answers on upgrade — the next save rewrites
/// them to YAML and `polling::sources_for` finds them there going forward.
pub fn load_persisted(store: &dyn Store) -> Option<PersistedSetup> {
    if let Some(p) = load_from_yaml(&config_yaml_path()) {
        return Some(p);
    }
    // Legacy kv fallback. Migrates by side-effect on the next save.
    match store.get_kv(KV_KEY_SETUP) {
        Ok(Some(raw)) if !raw.is_empty() => match serde_json::from_str::<PersistedSetup>(&raw) {
            Ok(mut p) => {
                p.migrate_legacy_keys();
                tracing::info!(
                    "setup loaded from legacy kv blob; will migrate to YAML on next save"
                );
                Some(p)
            }
            Err(e) => {
                tracing::warn!("legacy setup blob corrupt: {e}");
                None
            }
        },
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("legacy setup read failed: {e}");
            None
        }
    }
}

/// Persist setup state by merging into `~/.lazybox/config.yaml`, then clear
/// the legacy kv blob so the YAML stays the only source of truth on
/// subsequent loads. `Ok(Some(path))` reports a backup taken when a
/// malformed pre-existing config was moved aside; `Err` means nothing was
/// saved and the UI must say so.
pub fn save_persisted(
    store: &dyn Store,
    p: &PersistedSetup,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let backed_up = save_persisted_yaml(p, &config_yaml_path())?;
    let _ = store.set_kv(KV_KEY_SETUP, "");
    Ok(backed_up)
}
