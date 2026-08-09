//! # lazybox-sandbox
//!
//! A **generic remote dev-box abstraction**: start / stop / connect a box
//! that hosts a lazybox daemon and a project's workload, provider-agnostic,
//! with **GCP as the first implementation**.
//!
//! Two axes are kept deliberately separate (see epic #885, issue #931):
//!
//! 1. **Provider — box lifecycle.** The [`SandboxProvider`] trait: `ensure`
//!    (create if absent), `start`/`stop` (wake/sleep), `status`, `connect`
//!    (a port-forward tunnel), `destroy`. A [`BoxHandle`] is the persisted
//!    per-worktree pointer to a live box; see [`persist`] for the store
//!    round-trip.
//!
//! 2. **Deployment — what's on the box.** A [`Deployment`] recipe: a
//!    generic **default** (base toolchain + blank workspace) that a project
//!    **overrides** via base + overlay deep-merge — the same shape a
//!    `stack.local.yaml` overlay uses. obin's override is the box we
//!    already built (its repo + `dev up <profile>` + SA/IAM/NAT).
//!
//! Provisioning (`ensure`/`destroy`) drives a **Terraform module per
//! provider** (`terraform/sandbox/gcp` first). The fast lifecycle ops
//! (`start`/`stop`/`status`/`connect`) use the native SDK/CLI — never
//! Terraform — so waking a box is a `gcloud instances start`, not a plan.

mod connect;
mod deployment;
mod handle;
mod provider;
mod spec;

pub mod persist;

#[cfg(feature = "gcp")]
pub mod gcp;

pub use connect::connect_box;
pub use deployment::{Deployment, DeploymentConfig, merge_yaml};
pub use handle::{BoxHandle, BoxStatus, PowerState};
pub use provider::{SandboxError, SandboxProvider, Tunnel};
pub use spec::SandboxSpec;
