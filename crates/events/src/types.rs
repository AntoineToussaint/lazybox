use chrono::{DateTime, Utc};
use lazybox_core::{Activity, CiStatus, ReviewStatus, Task, TaskId, TaskState};

/// An event produced by any provider (GitHub, Linear, CI, …).
/// The app reacts to these generically — it never pattern-matches on
/// provider-specific data.
#[derive(Debug, Clone)]
pub struct Event {
    /// When the event was produced.
    pub timestamp: DateTime<Utc>,
    /// Which provider emitted it.
    pub source: String,
    /// The payload.
    pub kind: EventKind,
}

/// Source-agnostic event payloads.
#[derive(Debug, Clone)]
pub enum EventKind {
    /// A new or updated task was discovered. Boxed because `Task`
    /// is ~424 bytes while the other variants are <64 — without
    /// the box, every `EventKind` instance reserves the full Task
    /// footprint regardless of variant (clippy's
    /// `large_enum_variant` lint).
    TaskUpdated(Box<Task>),

    /// A task's state changed (e.g. merged, closed).
    TaskStateChanged {
        task_id: TaskId,
        old: TaskState,
        new: TaskState,
    },

    /// New activity on a task (comment, review, etc.).
    NewActivity { task_id: TaskId, activity: Activity },

    /// CI status changed.
    CiStatusChanged {
        task_id: TaskId,
        old: CiStatus,
        new: CiStatus,
    },

    /// Review status changed.
    ReviewStatusChanged {
        task_id: TaskId,
        old: ReviewStatus,
        new: ReviewStatus,
    },

    /// A task was removed (e.g. PR deleted, issue closed and you unfollowed).
    TaskRemoved(TaskId),

    /// Provider encountered an error (transient).
    ProviderError { message: String },
}

impl Event {
    pub fn new(source: impl Into<String>, kind: EventKind) -> Self {
        Self {
            timestamp: Utc::now(),
            source: source.into(),
            kind,
        }
    }

    /// Short human-readable summary for the notification area.
    pub fn summary(&self) -> String {
        match &self.kind {
            EventKind::TaskUpdated(t) => format!("Updated: {}", t.id),
            EventKind::TaskStateChanged { task_id, new, .. } => {
                format!("{task_id} → {new:?}")
            }
            EventKind::NewActivity { task_id, activity } => {
                format!(
                    "{}: {} from {}",
                    task_id,
                    activity_label(&activity.kind),
                    activity.author
                )
            }
            EventKind::CiStatusChanged { task_id, new, .. } => {
                format!("{task_id} CI → {new:?}")
            }
            EventKind::ReviewStatusChanged { task_id, new, .. } => {
                format!("{task_id} review → {new:?}")
            }
            EventKind::TaskRemoved(id) => format!("Removed: {id}"),
            EventKind::ProviderError { message } => format!("Error: {message}"),
        }
    }
}

fn activity_label(kind: &lazybox_core::ActivityKind) -> &'static str {
    match kind {
        lazybox_core::ActivityKind::Comment => "comment",
        lazybox_core::ActivityKind::Review => "review",
        lazybox_core::ActivityKind::StatusChange => "status",
        lazybox_core::ActivityKind::CiUpdate => "ci",
    }
}
