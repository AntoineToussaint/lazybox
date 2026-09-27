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
    receipt: oneshot::Receiver<DeliveryReceipt>,
    /// Fires the moment the gate lets the text through and it is written —
    /// before the submit-confirmation ladder, which can take ~30s. "It is in
    /// the agent's input" is what a sender needs to know first; "the agent
    /// started the turn" arrives later in the receipt.
    landed: oneshot::Receiver<()>,
}

/// The first thing that happens to a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EarlyOutcome {
    /// The text is being written into an agent that was ready for it.
    Landed,
    /// It was refused before landing.
    Refused { reason: String },
}

impl PendingDelivery {
    /// Wait at most `limit` for the text to land or be refused. `None` means
    /// it is still queued behind the gate; the handle stays usable.
    pub async fn landed_within(&mut self, limit: Duration) -> Option<EarlyOutcome> {
        let wait = async {
            tokio::select! {
                biased;
                landed = &mut self.landed => landed.is_ok().then_some(EarlyOutcome::Landed),
                receipt = &mut self.receipt => Some(match receipt {
                    Ok(DeliveryReceipt::Refused { reason }) => EarlyOutcome::Refused { reason },
                    Ok(DeliveryReceipt::Delivered { .. }) => EarlyOutcome::Landed,
                    Err(_) => EarlyOutcome::Refused {
                        reason: "the delivery ended without reporting an outcome".into(),
                    },
                }),
            }
        };
        tokio::time::timeout(limit, wait).await.ok().flatten()
    }

    /// Wait for the outcome. A dropped sender (the delivery task ended
    /// without reporting — a daemon bug, never a normal path) reads as a
    /// refusal rather than a hang.
    pub async fn receipt(self) -> DeliveryReceipt {
        self.receipt
            .await
            .unwrap_or_else(|_| DeliveryReceipt::Refused {
                reason: "the delivery ended without reporting an outcome".into(),
            })
    }

    /// Wait at most `limit`. `None` means the text is still queued — the
    /// gate is holding it (typically behind a busy agent) and it will land
    /// or be refused later; the handle stays usable, so the caller can hand
    /// it to a task that finishes the bookkeeping with [`Self::receipt`].
    pub async fn receipt_within(&mut self, limit: Duration) -> Option<DeliveryReceipt> {
        match tokio::time::timeout(limit, &mut self.receipt).await {
            Ok(Ok(receipt)) => Some(receipt),
            Ok(Err(_)) => Some(DeliveryReceipt::Refused {
                reason: "the delivery ended without reporting an outcome".into(),
            }),
            Err(_) => None,
        }
    }
}

/// Deliver `request` through the settle-gated inject path and hand back a
/// [`PendingDelivery`] for its receipt.
pub async fn deliver(config: &ServerConfig, request: DeliveryRequest) -> PendingDelivery {
    let (tx, rx) = oneshot::channel();
    let (landed_tx, landed_rx) = oneshot::channel();
    crate::spawn_handler::inject_with_receipt(config, request, ReceiptSlot::new(tx, landed_tx))
        .await;
    PendingDelivery {
        receipt: rx,
        landed: landed_rx,
    }
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

    /// The gate let the text through and it is about to be written.
    pub(crate) fn landing(&mut self) {
        if let Some(tx) = self.landed.take() {
            let _ = tx.send(());
        }
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
        let pending = PendingDelivery {
            receipt: rx,
            landed: landed_rx,
        };
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
        let pending = PendingDelivery {
            receipt: rx,
            landed: landed_rx,
        };
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
        let mut pending = PendingDelivery {
            receipt: rx,
            landed: landed_rx,
        };
        slot.landing();
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

    #[tokio::test]
    async fn an_unresolved_delivery_reads_as_still_queued() {
        let (tx, rx) = oneshot::channel::<DeliveryReceipt>();
        let (_landed_tx, landed_rx) = oneshot::channel();
        let mut pending = PendingDelivery {
            receipt: rx,
            landed: landed_rx,
        };
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
