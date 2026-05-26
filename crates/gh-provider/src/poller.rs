use crate::GhClient;
use pilot_core::Task;
use pilot_events::{Event, EventKind, EventProducer};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Polls GitHub via a single GraphQL query per cycle.
/// Detects changes between polls and emits fine-grained events.
pub struct GhPoller {
    client: GhClient,
    producer: EventProducer,
    interval: Duration,
    prev: HashMap<String, Task>,
    first_poll: bool,
    /// Notify to trigger an immediate poll (e.g. manual refresh).
    wake: Arc<Notify>,
}

impl GhPoller {
    pub fn new(client: GhClient, producer: EventProducer, interval: Duration) -> Self {
        Self {
            client,
            producer,
            interval,
            prev: HashMap::new(),
            first_poll: true,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Get a handle to wake the poller for an immediate poll.
    pub fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    pub async fn run(mut self) {
        info!(
            "GitHub poller started (interval: {:?}, user: {}, GraphQL mode)",
            self.interval,
            self.client.username()
        );

        loop {
            self.poll_cycle().await;
            // Sleep until interval OR wake signal (whichever comes first).
            tokio::select! {
                _ = tokio::time::sleep(self.interval) => {}
                _ = self.wake.notified() => {
                    info!("Poller woken for manual refresh");
                }
            }
        }
    }

    async fn poll_cycle(&mut self) {
        // One GraphQL call gets everything.
        let tasks = match self.client.fetch_all_prs().await {
            Ok(t) => t,
            Err(e) => {
                warn!("GitHub poll failed: {e}");
                self.producer.send(Event::new(
                    "github",
                    EventKind::ProviderError {
                        message: e.to_string(),
                    },
                ));
                return;
            }
        };

        let current: HashMap<String, Task> =
            tasks.into_iter().map(|t| (t.id.to_string(), t)).collect();

        info!(
            "Poll complete: {} PRs (first_poll={})",
            current.len(),
            self.first_poll
        );

        for (key, task) in &current {
            if self.first_poll {
                self.producer.send(Event::new(
                    "github",
                    EventKind::TaskUpdated(Box::new(task.clone())),
                ));
                continue;
            }

            if let Some(prev_task) = self.prev.get(key) {
                let mut changed = false;

                if prev_task.state != task.state {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::TaskStateChanged {
                            task_id: task.id.clone(),
                            old: prev_task.state,
                            new: task.state,
                        },
                    ));
                    changed = true;
                }

                if prev_task.ci != task.ci {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::CiStatusChanged {
                            task_id: task.id.clone(),
                            old: prev_task.ci,
                            new: task.ci,
                        },
                    ));
                    changed = true;
                }

                if prev_task.review != task.review {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::ReviewStatusChanged {
                            task_id: task.id.clone(),
                            old: prev_task.review,
                            new: task.review,
                        },
                    ));
                    changed = true;
                }

                if prev_task.title != task.title {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::NewActivity {
                            task_id: task.id.clone(),
                            activity: pilot_core::Activity {
                                author: "github".into(),
                                body: format!(
                                    "Title: \"{}\" → \"{}\"",
                                    prev_task.title, task.title
                                ),
                                created_at: chrono::Utc::now(),
                                kind: pilot_core::ActivityKind::StatusChange,
                                node_id: None,
                                path: None,
                                line: None,
                                diff_hunk: None,
                                thread_id: None,
                            },
                        },
                    ));
                    changed = true;
                }

                let prev_count = prev_task.recent_activity.len();
                let new_count = task.recent_activity.len();
                if new_count > prev_count {
                    for activity in task.recent_activity.iter().take(new_count - prev_count) {
                        self.producer.send(Event::new(
                            "github",
                            EventKind::NewActivity {
                                task_id: task.id.clone(),
                                activity: activity.clone(),
                            },
                        ));
                    }
                    changed = true;
                }

                if prev_task.needs_reply != task.needs_reply {
                    changed = true;
                }

                if changed || prev_task.updated_at != task.updated_at {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::TaskUpdated(Box::new(task.clone())),
                    ));
                }
            } else {
                debug!(task_id = %task.id, "New task");
                self.producer.send(Event::new(
                    "github",
                    EventKind::TaskUpdated(Box::new(task.clone())),
                ));
                for activity in &task.recent_activity {
                    self.producer.send(Event::new(
                        "github",
                        EventKind::NewActivity {
                            task_id: task.id.clone(),
                            activity: activity.clone(),
                        },
                    ));
                }
            }
        }

        // Detect removed tasks.
        for key in self.prev.keys() {
            if !current.contains_key(key)
                && let Some(prev_task) = self.prev.get(key)
            {
                self.producer.send(Event::new(
                    "github",
                    EventKind::TaskRemoved(prev_task.id.clone()),
                ));
            }
        }

        self.prev = current;
        self.first_poll = false;
    }
}
