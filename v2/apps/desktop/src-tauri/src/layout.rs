use crate::placement::Geometry;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_SURFACES: usize = 32;
pub const MAX_OPEN: usize = 16;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub session_id: String,
    pub command_id: String,
    pub commands_open: bool,
    pub collection_id: String,
    pub panel_id: String,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl Target {
    pub fn validate(&self) -> Result<(), String> {
        if (!self.session_id.is_empty() && !valid_id(&self.session_id))
            || (!self.collection_id.is_empty() && !valid_id(&self.collection_id))
            || (!self.panel_id.is_empty()
                && (!valid_id(&self.panel_id) || self.session_id.is_empty()))
            || (!self.command_id.is_empty()
                && (!valid_id(&self.command_id) || self.session_id.is_empty()))
        {
            return Err("Invalid session or command identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Surface {
    pub label: String,
    pub target: Target,
    pub open: bool,
    pub changed: u64,
    pub geometry: Option<Geometry>,
    pub maximized: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub version: u32,
    pub sequence: u64,
    pub surfaces: BTreeMap<String, Surface>,
}
impl Default for Layout {
    fn default() -> Self {
        let main = Surface {
            label: "main".into(),
            target: Target::default(),
            open: true,
            changed: 0,
            geometry: None,
            maximized: false,
        };
        Self {
            version: 1,
            sequence: 0,
            surfaces: BTreeMap::from([("main".into(), main)]),
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    #[test]
    fn workspace_window_keeps_its_workspace_and_can_change_conversations() {
        let mut layout = Layout::default();
        let target = Target {
            collection_id: "feature-one".into(),
            session_id: "session-one".into(),
            ..Target::default()
        };
        let label = layout.workspace(target.clone()).expect("workspace");
        layout.mark_open(&label).expect("open");
        assert_eq!(layout.workspace(target.clone()).expect("reuse"), label);
        let mut next = target.clone();
        next.session_id = "session-two".into();
        layout
            .save_target(&label, next.clone())
            .expect("switch session");
        next.collection_id = "another-workspace".into();
        assert!(layout.save_target(&label, next).is_err());
        layout.validate().expect("valid layout");
        let restored: Layout =
            serde_json::from_str(&serde_json::to_string(&layout).expect("serialize"))
                .expect("restore");
        restored.validate().expect("valid restored layout");
        assert_eq!(restored.surfaces[&label].target.session_id, "session-two");
        assert!(restored.surfaces[&label].open);
    }
}
impl Layout {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.surfaces.len() > MAX_SURFACES
            || !self.surfaces.contains_key("main")
            || self.surfaces.values().filter(|s| s.open).count() > MAX_OPEN
        {
            return Err("Unsupported or oversized saved window layout".into());
        }
        for (label, surface) in &self.surfaces {
            surface.target.validate()?;
            let identity_matches = match label.as_str() {
                "main" => surface.target.panel_id.is_empty(),
                _ if label.starts_with("workspace-") => {
                    !surface.target.collection_id.is_empty()
                        && surface.target.panel_id.is_empty()
                        && label == &format!("workspace-{}", surface.target.collection_id)
                }
                _ if label.starts_with("console-") => {
                    !surface.target.command_id.is_empty()
                        && surface.target.panel_id.is_empty()
                        && label == &format!("console-{}", surface.target.command_id)
                }
                _ if label.starts_with("chat-") => {
                    !surface.target.panel_id.is_empty()
                        && surface.target.command_id.is_empty()
                        && label == &format!("chat-{}", surface.target.panel_id)
                }
                _ => false,
            };
            if label != &surface.label
                || !identity_matches
                || surface.geometry.as_ref().is_some_and(|g| !g.valid())
            {
                return Err("Invalid saved window surface".into());
            }
        }
        Ok(())
    }
    pub fn touch(&mut self, label: &str) -> Result<&mut Surface, String> {
        self.sequence = self.sequence.saturating_add(1);
        let surface = self
            .surfaces
            .get_mut(label)
            .ok_or("Unknown window surface")?;
        surface.changed = self.sequence;
        Ok(surface)
    }
    pub fn save_target(&mut self, label: &str, target: Target) -> Result<(), String> {
        target.validate()?;
        let current = self.surfaces.get(label).ok_or("Unknown window surface")?;
        if label.starts_with("workspace-") {
            if target.collection_id != current.target.collection_id || !target.panel_id.is_empty() {
                return Err("A workspace window retains its workspace identity".into());
            }
            self.touch(label)?.target = target;
            return Ok(());
        }
        if label != "main"
            && (target.session_id != current.target.session_id
                || target.command_id != current.target.command_id
                || target.collection_id != current.target.collection_id
                || target.panel_id != current.target.panel_id)
        {
            return Err("A detached viewer cannot change its execution identity".into());
        }
        self.touch(label)?.target = target;
        Ok(())
    }
    pub fn console(&mut self, target: Target) -> Result<String, String> {
        target.validate()?;
        if target.command_id.is_empty() {
            return Err("Console command identity required".into());
        }
        let label = format!("console-{}", target.command_id);
        if let Some(existing) = self.surfaces.get(&label) {
            if existing.target.session_id != target.session_id {
                return Err("Console belongs to another execution".into());
            }
            return Ok(label);
        }
        if self.surfaces.len() == MAX_SURFACES {
            let oldest = self
                .surfaces
                .values()
                .filter(|s| !s.open && s.label != "main")
                .min_by_key(|s| s.changed)
                .map(|s| s.label.clone())
                .ok_or("Saved window limit reached")?;
            self.surfaces.remove(&oldest);
        }
        self.surfaces.insert(
            label.clone(),
            Surface {
                label: label.clone(),
                target,
                open: false,
                changed: self.sequence,
                geometry: None,
                maximized: false,
            },
        );
        Ok(label)
    }
    pub fn chat(&mut self, target: Target) -> Result<String, String> {
        target.validate()?;
        if target.panel_id.is_empty() || !target.command_id.is_empty() {
            return Err("Chat panel and session identities required".into());
        }
        let label = format!("chat-{}", target.panel_id);
        if let Some(existing) = self.surfaces.get(&label) {
            if existing.open && existing.target != target {
                return Err("Dock the existing chat before changing its target".into());
            }
            self.touch(&label)?.target = target;
            return Ok(label);
        }
        if self.surfaces.len() >= MAX_SURFACES {
            let oldest = self
                .surfaces
                .values()
                .filter(|s| !s.open && s.label != "main")
                .min_by_key(|s| s.changed)
                .map(|s| s.label.clone())
                .ok_or("Saved window limit reached")?;
            self.surfaces.remove(&oldest);
        }
        self.surfaces.insert(
            label.clone(),
            Surface {
                label: label.clone(),
                target,
                open: false,
                changed: self.sequence,
                geometry: None,
                maximized: false,
            },
        );
        Ok(label)
    }
    pub fn workspace(&mut self, target: Target) -> Result<String, String> {
        target.validate()?;
        if target.collection_id.is_empty() || !target.panel_id.is_empty() {
            return Err("Workspace identity required".into());
        }
        let label = format!("workspace-{}", target.collection_id);
        if self.surfaces.contains_key(&label) {
            return Ok(label);
        }
        if self.surfaces.len() >= MAX_SURFACES {
            let oldest = self
                .surfaces
                .values()
                .filter(|s| !s.open && s.label != "main")
                .min_by_key(|s| s.changed)
                .map(|s| s.label.clone())
                .ok_or("Saved window limit reached")?;
            self.surfaces.remove(&oldest);
        }
        self.surfaces.insert(
            label.clone(),
            Surface {
                label: label.clone(),
                target,
                open: false,
                changed: self.sequence,
                geometry: None,
                maximized: false,
            },
        );
        Ok(label)
    }
    pub fn mark_open(&mut self, label: &str) -> Result<(), String> {
        if !self
            .surfaces
            .get(label)
            .ok_or("Unknown window surface")?
            .open
            && self.surfaces.values().filter(|s| s.open).count() >= MAX_OPEN
        {
            return Err("At most 16 native windows can be open".into());
        }
        self.touch(label)?.open = true;
        Ok(())
    }
    pub fn close(&mut self, label: &str) -> Result<(), String> {
        self.touch(label)?.open = false;
        Ok(())
    }
    pub fn last_closed(&self) -> Option<String> {
        self.surfaces
            .values()
            .filter(|s| !s.open)
            .max_by_key(|s| s.changed)
            .map(|s| s.label.clone())
    }
    pub fn survivor(&self, closed: &str) -> Option<String> {
        self.surfaces
            .values()
            .filter(|s| s.open && s.label != closed)
            .max_by_key(|s| s.changed)
            .map(|s| s.label.clone())
    }
}

pub struct Repository {
    pub path: PathBuf,
}
impl Repository {
    pub fn load(&self) -> Result<Layout, String> {
        let data = match fs::read(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Layout::default());
            }
            Err(error) => return Err(error.to_string()),
        };
        if data.len() > 1 << 20 {
            return Err("Saved window layout exceeds its size limit".into());
        }
        let layout: Layout = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        layout.validate()?;
        Ok(layout)
    }
    pub fn save(&self, layout: &Layout) -> Result<(), String> {
        layout.validate()?;
        let parent = self
            .path
            .parent()
            .ok_or("Window layout has no parent directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, layout).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&self.path).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn new(root: &Path, authority: &str) -> Self {
        Self {
            path: root.join(format!("windows-{authority}.json")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(command: &str) -> Target {
        Target {
            session_id: "session".into(),
            command_id: command.into(),
            commands_open: true,
            ..Target::default()
        }
    }
    #[test]
    fn close_reopen_preserves_targets_and_selects_a_surviving_window() {
        let mut layout = Layout::default();
        layout
            .save_target("main", target("command"))
            .expect("target");
        let label = layout.console(target("command")).expect("console");
        layout.mark_open(&label).expect("open");
        assert_eq!(layout.console(target("command")).expect("reuse"), label);
        layout.close("main").expect("close host");
        assert_eq!(layout.survivor("main"), Some(label.clone()));
        layout.close(&label).expect("close viewer");
        assert_eq!(layout.last_closed(), Some(label.clone()));
        layout.mark_open(&label).expect("reopen");
        assert_eq!(layout.surfaces[&label].target, target("command"));
        assert!(!layout.surfaces["main"].open);
        assert!(layout.save_target(&label, target("other")).is_err());
    }
    #[test]
    fn persistence_survives_a_host_restart_and_rejects_corruption() {
        let root = tempfile::tempdir().expect("directory");
        let repository = Repository::new(root.path(), "authority-a");
        let mut layout = repository.load().expect("initial");
        layout
            .save_target("main", target("command"))
            .expect("target");
        repository.save(&layout).expect("save");
        let restored = repository.load().expect("restore");
        assert_eq!(restored.surfaces["main"].target, target("command"));
        assert!(
            Repository::new(root.path(), "authority-b")
                .load()
                .expect("other authority")
                .surfaces["main"]
                .target
                .session_id
                .is_empty()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&repository.path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::write(&repository.path, b"broken").expect("corrupt fixture");
        assert!(repository.load().is_err());
        assert_eq!(fs::read(&repository.path).expect("preserved"), b"broken");
    }
    #[test]
    fn identities_and_window_counts_are_bounded() {
        let mut layout = Layout::default();
        for invalid in ["../path", "x&token=secret", "x?redirect=other", "x#secret"] {
            assert!(layout.console(target(invalid)).is_err());
        }
        for i in 0..MAX_OPEN - 1 {
            let label = layout
                .console(target(&format!("command-{i}")))
                .expect("console");
            layout.mark_open(&label).expect("open");
        }
        let label = layout.console(target("overflow")).expect("record");
        assert!(layout.mark_open(&label).is_err());
        for i in 0..80 {
            let label = layout
                .console(target(&format!("closed-{i}")))
                .expect("record");
            layout.close(&label).expect("close");
        }
        assert!(layout.surfaces.len() <= MAX_SURFACES);
        assert!(layout.validate().is_ok());
    }
    #[test]
    fn detached_chat_reuses_its_view_and_cannot_redirect_a_live_target() {
        let mut layout = Layout::default();
        let target = Target {
            panel_id: "work-chat".into(),
            session_id: "session-a".into(),
            collection_id: "collection-a".into(),
            ..Target::default()
        };
        let label = layout.chat(target.clone()).expect("chat");
        layout.mark_open(&label).expect("open");
        assert_eq!(layout.chat(target.clone()).expect("reuse"), label);
        assert_eq!(layout.surfaces.len(), 2);
        assert!(
            layout
                .chat(Target {
                    session_id: "session-b".into(),
                    ..target.clone()
                })
                .is_err()
        );
        assert!(
            layout
                .save_target(
                    &label,
                    Target {
                        collection_id: "collection-b".into(),
                        ..target.clone()
                    }
                )
                .is_err()
        );
        layout.close(&label).expect("dock");
        assert_eq!(layout.surfaces[&label].target, target);
        assert!(layout.validate().is_ok());
        assert!(
            layout
                .chat(Target {
                    panel_id: "../escape".into(),
                    ..target.clone()
                })
                .is_err()
        );
        assert!(
            layout
                .chat(Target {
                    session_id: String::new(),
                    ..target
                })
                .is_err()
        );
    }
    #[test]
    fn legacy_targets_load_without_losing_execution_references() {
        let target: Target = serde_json::from_str(
            r#"{"sessionId":"existing","commandId":"command","commandsOpen":true}"#,
        )
        .expect("old target");
        assert_eq!(target.session_id, "existing");
        assert_eq!(target.command_id, "command");
        assert!(target.panel_id.is_empty());
        assert!(target.collection_id.is_empty());
    }
}
