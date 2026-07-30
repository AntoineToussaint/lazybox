use lazybox_config::ActivityPaneMode;
use lazybox_core::WorkspaceKey;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(super) struct ActivityPaneState {
    overrides: HashMap<WorkspaceKey, ActivityPaneMode>,
}

impl ActivityPaneState {
    pub(super) fn mode(
        &self,
        workspace: Option<&WorkspaceKey>,
        has_visible_content: bool,
        default: ActivityPaneMode,
    ) -> ActivityPaneMode {
        let Some(workspace) = workspace else {
            return ActivityPaneMode::Full;
        };
        if let Some(&mode) = self.overrides.get(workspace) {
            return mode;
        }
        if !has_visible_content {
            return ActivityPaneMode::Hidden;
        }
        default
    }

    pub(super) fn visible(
        &self,
        workspace: Option<&WorkspaceKey>,
        has_visible_content: bool,
        default: ActivityPaneMode,
    ) -> bool {
        self.mode(workspace, has_visible_content, default) == ActivityPaneMode::Full
    }

    pub(super) fn cycle(
        &mut self,
        workspace: WorkspaceKey,
        has_visible_content: bool,
        default: ActivityPaneMode,
    ) -> ActivityPaneMode {
        let next = match self.mode(Some(&workspace), has_visible_content, default) {
            ActivityPaneMode::Full => ActivityPaneMode::Summary,
            ActivityPaneMode::Summary => ActivityPaneMode::Hidden,
            ActivityPaneMode::Hidden => ActivityPaneMode::Full,
        };
        self.overrides.insert(workspace, next);
        next
    }

    pub(super) fn expand(&mut self, workspace: WorkspaceKey) {
        self.overrides.insert(workspace, ActivityPaneMode::Full);
    }

    pub(super) fn forget(&mut self, workspace: &WorkspaceKey) {
        self.overrides.remove(workspace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> WorkspaceKey {
        WorkspaceKey::new(value)
    }

    #[test]
    fn no_selection_stays_full() {
        let state = ActivityPaneState::default();

        assert_eq!(
            state.mode(None, false, ActivityPaneMode::Hidden),
            ActivityPaneMode::Full
        );
        assert!(state.visible(None, false, ActivityPaneMode::Hidden));
    }

    #[test]
    fn content_uses_the_configured_default() {
        let state = ActivityPaneState::default();
        let workspace = key("github:o/r#1");

        assert_eq!(
            state.mode(Some(&workspace), true, ActivityPaneMode::Summary),
            ActivityPaneMode::Summary
        );
        assert!(!state.visible(Some(&workspace), true, ActivityPaneMode::Summary));
    }

    #[test]
    fn empty_workspace_is_hidden_before_an_override() {
        let state = ActivityPaneState::default();
        let workspace = key("github:o/r#1");

        assert_eq!(
            state.mode(Some(&workspace), false, ActivityPaneMode::Full),
            ActivityPaneMode::Hidden
        );
    }

    #[test]
    fn cycle_is_remembered_per_workspace() {
        let mut state = ActivityPaneState::default();
        let first = key("github:o/r#1");
        let second = key("github:o/r#2");

        assert_eq!(
            state.cycle(first.clone(), false, ActivityPaneMode::Full),
            ActivityPaneMode::Full
        );
        assert!(state.visible(Some(&first), false, ActivityPaneMode::Hidden));
        assert_eq!(
            state.mode(Some(&second), false, ActivityPaneMode::Full),
            ActivityPaneMode::Hidden
        );
        assert_eq!(
            state.cycle(first.clone(), false, ActivityPaneMode::Full),
            ActivityPaneMode::Summary
        );
        assert_eq!(
            state.cycle(first.clone(), false, ActivityPaneMode::Full),
            ActivityPaneMode::Hidden
        );
        assert_eq!(
            state.cycle(first, false, ActivityPaneMode::Full),
            ActivityPaneMode::Full
        );
    }

    #[test]
    fn expand_and_forget_update_the_override() {
        let mut state = ActivityPaneState::default();
        let workspace = key("github:o/r#1");

        state.expand(workspace.clone());
        assert!(state.visible(Some(&workspace), false, ActivityPaneMode::Hidden));

        state.forget(&workspace);
        assert_eq!(
            state.mode(Some(&workspace), false, ActivityPaneMode::Full),
            ActivityPaneMode::Hidden
        );
    }
}
