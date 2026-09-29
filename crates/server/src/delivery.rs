//! One path for putting text in front of an agent, and a receipt back.
//!
//! lazybox grew seven ways to deliver text into an agent's terminal — the
//! `w w` inject, snippets, spawn prompts, broadcast, resume, auto-fix and
//! credit recovery — each with its own readiness gate and its own meaning of
//! "delivered", and every one of them returned `()` to whoever asked. The
//! outcome reached the TUI as a toast and nobody else: an agent that
//! `notify_session`-ed a sibling was told only that the text was "handed
//! off", and an `ask_session` could be answered by the end of a turn the
//! target was already running when the question arrived
//! (`docs/agent-coordination-v2.md`).
//!
//! This module is the owner those paths converge on. A [`DeliveryRequest`]
//! names who is speaking ([`Party`]), how long to wait for the agent to be
//! ready ([`Gate`]) and whether to submit; [`deliver`] runs the existing
//! settle-gated inject and resolves a [`DeliveryReceipt`] at every exit, so a
//! caller can finally tell "the agent started the turn" from "refused" from
//! "still queued behind a busy agent".

use lazybox_core::SessionKey;
use lazybox_ipc::TerminalId;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::ServerConfig;

/// Who is putting text in front of the agent. Recorded so the receiving side
/// — and later its history — can tell a human's prompt from lazybox's own
/// automation from a sibling agent's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Party {
    /// The user, through the TUI, the desktop app or the CLI.
    Human,
    /// Another agent session, through the coordination tools.
    Agent(SessionKey),
    /// lazybox itself (auto-fix, resume, epic dispatch), with a short reason.
    Lazybox(&'static str),
}

/// When the text may land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Wait only while a chooser / permission prompt owns the input, then
    /// paste — even into an agent mid-turn, whose CLI queues it. The
    /// behaviour of a human's `w w`, which people rely on to queue the next
    /// instruction behind the current one.
    ChooserOnly,
    /// Also wait until the agent is not mid-turn. For agent-originated
    /// messages: pasting into a working agent interleaves the message with
    /// a turn about something else, and the turn's end then looks like an
    /// answer to a question it never considered.
    Idle,
}

/// A request to deliver `body` into the agent on `terminal_id`.
#[derive(Debug, Clone)]
pub struct DeliveryRequest {
    pub terminal_id: TerminalId,
    pub body: String,
    /// Paste and submit (true) or leave it composed for a human (false).
    pub submit: bool,
    pub gate: Gate,
    pub from: Party,
    /// How long the gate may hold the text before giving up. `None` keeps
    /// the inject path's own deadline.
    pub wait_limit: Option<Duration>,
}

/// What happened to a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryReceipt {
    /// The text is in the agent's input. `confirmed` is whether the agent
    /// acknowledged the submit (its `UserPromptSubmit` hook or a `Working`
    /// transition); a compose-only delivery is never "confirmed".
    Delivered { confirmed: bool },
    /// It was not delivered, and why — in words a user or agent can act on.
    Refused { reason: String },
}

/// A delivery in flight. The text is registered once this is returned; the
/// receipt resolves when it lands or is refused.
pub struct PendingDelivery {
    /// `None` once the one-shot has reported. Every reader here may be called
    /// twice — an `ask_session` that finds the text still queued hands the
    /// handle to a task that reads it again — and a completed
    /// `oneshot::Receiver` panics with "called after complete" when polled
    /// again, so a receiver is taken out the moment it resolves and the
    /// outcome is kept below instead.
    receipt: Option<oneshot::Receiver<DeliveryReceipt>>,
    /// Fires the moment the gate lets the text through and it is written —
    /// before the submit-confirmation ladder, which can take ~30s. "It is in
    /// the agent's input" is what a sender needs to know first; "the agent
    /// started the turn" arrives later in the receipt.
    landed: Option<oneshot::Receiver<()>>,
    /// The receipt once seen, so a handle read twice reports the same outcome
    /// twice rather than re-polling a spent one-shot or reading as queued.
    outcome: Option<DeliveryReceipt>,
    /// Likewise for the early outcome.
    early: Option<EarlyOutcome>,
}

/// The first thing that happens to a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EarlyOutcome {
    /// The text is being written into an agent that was ready for it.
    Landed,
    /// It was refused before landing.
    Refused { reason: String },
}

/// What a single wait on the two one-shots settled.
enum Step {
    /// The `landed` signal fired: the text is in the composer.
    Landed,
    /// The `landed` sender was dropped unsent — it never landed. Not an
    /// outcome on its own: the receipt carries the refusal that says why.
    NeverLanded,
    /// The receipt reported.
    Reported(DeliveryReceipt),
}

impl PendingDelivery {
    pub(crate) fn new(
        receipt: oneshot::Receiver<DeliveryReceipt>,
        landed: oneshot::Receiver<()>,
    ) -> Self {
        Self {
            receipt: Some(receipt),
            landed: Some(landed),
            outcome: None,
            early: None,
        }
    }

    /// Wait at most `limit` for the text to land or be refused. `None` means
    /// it is still queued behind the gate; the handle stays usable, and a
    /// second call reports the same outcome rather than re-polling a spent
    /// one-shot.
    pub async fn landed_within(&mut self, limit: Duration) -> Option<EarlyOutcome> {
        tokio::time::timeout(limit, self.early_outcome())
            .await
            .ok()
            .flatten()
    }

    /// The first outcome, however long it takes — bounded by the caller's own
    /// `timeout`. `None` only when there is nothing left to wait on.
    async fn early_outcome(&mut self) -> Option<EarlyOutcome> {
        loop {
            if let Some(early) = &self.early {
                return Some(early.clone());
            }
            if let Some(outcome) = &self.outcome {
                let early = EarlyOutcome::of(outcome);
                self.early = Some(early.clone());
                return Some(early);
            }
            // Resolve first, mutate after: the arms below borrow both fields.
            let step = match (self.landed.as_mut(), self.receipt.as_mut()) {
                // `biased` matters twice over: the landing is the signal a
                // sender wants first, and a select that polled an already-ready
                // receipt and then discarded it would drop the outcome.
                (Some(landed), Some(receipt)) => tokio::select! {
                    biased;
                    signal = landed => if signal.is_ok() { Step::Landed } else { Step::NeverLanded },
                    reported = receipt => Step::Reported(DeliveryReceipt::or_unreported(reported)),
                },
                (None, Some(receipt)) => {
                    Step::Reported(DeliveryReceipt::or_unreported(receipt.await))
                }
                // No receipt left and no outcome kept: unreachable, since the
                // receiver is only taken out together with its outcome. It
                // degrades to the same refusal `receipt_within` gives for this
                // state, never to `None` — `None` means "still queued", which
                // is the branch that spawns a task to read the handle again,
                // and sending an impossible state down the path that waits for
                // something that can never arrive is the shape of the bug this
                // whole change fixes.
                (Some(_), None) | (None, None) => {
                    return Some(EarlyOutcome::of(&DeliveryReceipt::unreported()));
                }
            };
            match step {
                Step::Landed => {
                    self.landed = None;
                    self.early = Some(EarlyOutcome::Landed);
                    return Some(EarlyOutcome::Landed);
                }
                // It never landed, so stop consulting that signal — but the
                // delivery is not "still queued": wait for the receipt to say
                // why, within the caller's limit.
                Step::NeverLanded => self.landed = None,
                Step::Reported(outcome) => {
                    self.receipt = None;
                    self.outcome = Some(outcome);
                }
            }
        }
    }

    /// Wait for the outcome. A dropped sender (the delivery task ended
    /// without reporting — a daemon bug, never a normal path) reads as a
    /// refusal rather than a hang.
    pub async fn receipt(mut self) -> DeliveryReceipt {
        if let Some(outcome) = self.outcome.take() {
            return outcome;
        }
        match self.receipt.take() {
            Some(receipt) => DeliveryReceipt::or_unreported(receipt.await),
            None => DeliveryReceipt::unreported(),
        }
    }

    /// Wait at most `limit`. `None` means the text is still queued — the
    /// gate is holding it (typically behind a busy agent) and it will land
    /// or be refused later; the handle stays usable, so the caller can hand
    /// it to a task that finishes the bookkeeping with [`Self::receipt`].
    pub async fn receipt_within(&mut self, limit: Duration) -> Option<DeliveryReceipt> {
        if let Some(outcome) = &self.outcome {
            return Some(outcome.clone());
        }
        let reported = {
            let Some(receipt) = self.receipt.as_mut() else {
                return Some(DeliveryReceipt::unreported());
            };
            match tokio::time::timeout(limit, receipt).await {
                Ok(reported) => reported,
                Err(_) => return None,
            }
        };
        let outcome = DeliveryReceipt::or_unreported(reported);
        self.receipt = None;
        self.outcome = Some(outcome.clone());
        Some(outcome)
    }
}

impl EarlyOutcome {
    /// What a settled receipt says about the landing.
    fn of(receipt: &DeliveryReceipt) -> Self {
        match receipt {
            DeliveryReceipt::Delivered { .. } => Self::Landed,
            DeliveryReceipt::Refused { reason } => Self::Refused {
                reason: reason.clone(),
            },
        }
    }
}

impl DeliveryReceipt {
    /// The delivery ended without reporting — a daemon bug, never a normal
    /// path, and a refusal rather than a hang wherever it is read.
    fn unreported() -> Self {
        Self::Refused {
            reason: "the delivery ended without reporting an outcome".into(),
        }
    }

    fn or_unreported(reported: Result<Self, oneshot::error::RecvError>) -> Self {
        reported.unwrap_or_else(|_| Self::unreported())
    }
}

/// Deliver `request` through the settle-gated inject path and hand back a
/// [`PendingDelivery`] for its receipt.
pub async fn deliver(config: &ServerConfig, request: DeliveryRequest) -> PendingDelivery {
    let (tx, rx) = oneshot::channel();
    let (landed_tx, landed_rx) = oneshot::channel();
    crate::spawn_handler::inject_with_receipt(config, request, ReceiptSlot::new(tx, landed_tx))
        .await;
    PendingDelivery::new(rx, landed_rx)
}

/// The one-shots a delivery reports into, each consumed by its first outcome.
pub(crate) struct ReceiptSlot {
    receipt: Option<oneshot::Sender<DeliveryReceipt>>,
    landed: Option<oneshot::Sender<()>>,
}

impl ReceiptSlot {
    pub(crate) fn new(
        receipt: oneshot::Sender<DeliveryReceipt>,
        landed: oneshot::Sender<()>,
    ) -> Self {
        Self {
            receipt: Some(receipt),
            landed: Some(landed),
        }
    }

    /// A slot nobody is listening on — the legacy fire-and-forget callers.
    pub(crate) fn none() -> Self {
        Self {
            receipt: None,
            landed: None,
        }
    }

    /// Hand the `landed` signal to whoever will know the initial write
    /// succeeded, so the slot itself stays free for the outcome.
    ///
    /// The signal belongs to the write, not to winning the readiness gate:
    /// a delivery whose write then failed used to report `Landed` — and
    /// commit a prompt-history row — for text that never reached the
    /// composer, while its receipt correctly refused. Dropping the returned
    /// sender unsent is the "never landed" case, which
    /// [`PendingDelivery::landed_within`] reads as no early outcome.
    pub(crate) fn take_landed(&mut self) -> Option<oneshot::Sender<()>> {
        self.landed.take()
    }

    pub(crate) fn resolve(&mut self, receipt: DeliveryReceipt) {
        if let Some(tx) = self.receipt.take() {
            let _ = tx.send(receipt);
        }
    }

    pub(crate) fn refuse(&mut self, reason: impl Into<String>) {
        self.resolve(DeliveryReceipt::Refused {
            reason: reason.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_first_outcome_wins_and_later_ones_are_ignored() {
        let (tx, rx) = oneshot::channel();
        let (landed_tx, landed_rx) = oneshot::channel();
        let mut slot = ReceiptSlot::new(tx, landed_tx);
        slot.resolve(DeliveryReceipt::Delivered { confirmed: true });
        slot.refuse("too late");
        let pending = PendingDelivery::new(rx, landed_rx);
        assert_eq!(
            pending.receipt().await,
            DeliveryReceipt::Delivered { confirmed: true }
        );
    }

    #[tokio::test]
    async fn a_delivery_that_never_reports_reads_as_refused_not_a_hang() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (_landed_tx, landed_rx) = oneshot::channel();
        drop(tx);
        let pending = PendingDelivery::new(rx, landed_rx);
        assert!(matches!(
            pending.receipt().await,
            DeliveryReceipt::Refused { .. }
        ));
    }

    #[tokio::test]
    async fn landing_is_reported_before_the_submit_is_confirmed() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (landed_tx, landed_rx) = oneshot::channel();
        let mut slot = ReceiptSlot::new(tx, landed_tx);
        let mut pending = PendingDelivery::new(rx, landed_rx);
        let _ = slot.take_landed().expect("the landed signal").send(());
        assert_eq!(
            pending.landed_within(Duration::from_secs(1)).await,
            Some(EarlyOutcome::Landed)
        );
        slot.resolve(DeliveryReceipt::Delivered { confirmed: true });
        assert_eq!(
            pending.receipt().await,
            DeliveryReceipt::Delivered { confirmed: true }
        );
    }

    /// The panic this module shipped: `ask_session` in async mode reads the
    /// handle, finds the text queued, and hands it to a task that reads it
    /// again — and a `oneshot::Receiver` polled after it completed panics with
    /// "called after complete", killing the task that would have marked the
    /// request delivered or abandoned.
    #[tokio::test]
    async fn a_handle_read_twice_never_repolls_a_spent_one_shot() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (landed_tx, landed_rx) = oneshot::channel();
        // The write failed, so the landing signal is dropped unsent: the
        // `landed` receiver completes, and the old reader returned `None`
        // ("still queued") while leaving it spent.
        drop(landed_tx);
        let mut pending = PendingDelivery::new(rx, landed_rx);
        assert_eq!(pending.landed_within(Duration::from_millis(20)).await, None);
        assert_eq!(
            pending.landed_within(Duration::from_millis(20)).await,
            None,
            "the second read panicked instead of answering",
        );
        tx.send(DeliveryReceipt::Refused {
            reason: "the composer never took the text".into(),
        })
        .expect("the receipt receiver is still alive");
        assert_eq!(
            pending.landed_within(Duration::from_secs(1)).await,
            Some(EarlyOutcome::Refused {
                reason: "the composer never took the text".into()
            }),
        );
    }

    /// The two readers must agree about the state that cannot happen. `None`
    /// from `landed_within` means "still queued", which is the branch that
    /// spawns a task to read the handle again — so an impossible state must
    /// never be reported as queued, whatever `receipt_within` would say.
    #[tokio::test]
    async fn an_impossible_state_terminates_in_both_readers() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (landed_tx, landed_rx) = oneshot::channel();
        drop(landed_tx);
        drop(tx);
        let mut pending = PendingDelivery::new(rx, landed_rx);
        // Both one-shots are closed with nothing sent: neither reader can ever
        // learn an outcome, so both must terminate rather than say "queued".
        assert!(matches!(
            pending.landed_within(Duration::from_millis(20)).await,
            Some(EarlyOutcome::Refused { .. })
        ));
        assert!(matches!(
            pending.receipt_within(Duration::from_millis(20)).await,
            Some(DeliveryReceipt::Refused { .. })
        ));
    }

    /// A write that failed has a refusal waiting on the receipt. Reporting it
    /// as "still queued" sent an asker away believing its question was behind
    /// a busy agent, and left the target badged with a question it never saw.
    #[tokio::test]
    async fn a_failed_write_reports_its_refusal_not_still_queued() {
        let (tx, rx) = oneshot::channel();
        let (landed_tx, landed_rx) = oneshot::channel();
        let mut slot = ReceiptSlot::new(tx, landed_tx);
        drop(slot.take_landed().expect("the landed signal"));
        slot.refuse("the composer never took the text");
        let mut pending = PendingDelivery::new(rx, landed_rx);
        assert_eq!(
            pending.landed_within(Duration::from_millis(20)).await,
            Some(EarlyOutcome::Refused {
                reason: "the composer never took the text".into()
            }),
        );
    }

    /// `notify_session` reads the early outcome, then hands the handle to a
    /// task that logs the final one. Whichever of the two one-shots the early
    /// read consumed, the outcome is still there afterwards.
    #[tokio::test]
    async fn an_outcome_consumed_by_an_early_read_is_still_reported() {
        let (tx, rx) = oneshot::channel();
        let (landed_tx, landed_rx) = oneshot::channel();
        let mut slot = ReceiptSlot::new(tx, landed_tx);
        drop(slot.take_landed().expect("the landed signal"));
        slot.refuse("no terminal");
        let mut pending = PendingDelivery::new(rx, landed_rx);
        assert!(matches!(
            pending.landed_within(Duration::from_millis(20)).await,
            Some(EarlyOutcome::Refused { .. })
        ));
        assert_eq!(
            pending.receipt().await,
            DeliveryReceipt::Refused {
                reason: "no terminal".into()
            },
            "the early read swallowed the receipt",
        );
    }

    /// A landing already reported reads the same the second time, and does not
    /// consume the receipt that still has to confirm the submit.
    #[tokio::test]
    async fn a_landing_reads_the_same_every_time() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (landed_tx, landed_rx) = oneshot::channel();
        let mut slot = ReceiptSlot::new(tx, landed_tx);
        let mut pending = PendingDelivery::new(rx, landed_rx);
        slot.take_landed()
            .expect("the landed signal")
            .send(())
            .expect("the landing receiver is still alive");
        for _ in 0..3 {
            assert_eq!(
                pending.landed_within(Duration::from_secs(1)).await,
                Some(EarlyOutcome::Landed),
            );
        }
        slot.resolve(DeliveryReceipt::Delivered { confirmed: true });
        assert_eq!(
            pending.receipt().await,
            DeliveryReceipt::Delivered { confirmed: true },
        );
    }

    #[tokio::test]
    async fn an_unresolved_delivery_reads_as_still_queued() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (_landed_tx, landed_rx) = oneshot::channel();
        let mut pending = PendingDelivery::new(rx, landed_rx);
        assert_eq!(
            pending.receipt_within(Duration::from_millis(20)).await,
            None
        );
        // Still usable after a timed-out wait: the late outcome arrives.
        tx.send(DeliveryReceipt::Delivered { confirmed: true })
            .unwrap();
        assert_eq!(
            pending.receipt().await,
            DeliveryReceipt::Delivered { confirmed: true }
        );
    }
}
