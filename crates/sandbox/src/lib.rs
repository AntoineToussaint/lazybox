//! # lazybox-sandbox
//!
//! A **generic remote dev-box abstraction**: start / stop / connect a box
//! that hosts a lazybox daemon and a project's workload, provider-agnostic,
//! with GCP and E2B implementations plus a managed-Kubernetes control-plane
//! contract.
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
//!    `stack.local.yaml` overlay uses. Product-specific recipes stay in the
//!    product repository; lazybox owns only the generic engine and schema.
//!
//! Provisioning (`ensure`/`destroy`) uses each provider's create/delete
//! boundary. The fast lifecycle ops (`start`/`stop`/`status`/`connect`) use
//! the native API/CLI, so waking a box never requires a Terraform plan.

mod connect;
mod deployment;
mod handle;
mod provider;
mod spec;

pub mod persist;

#[cfg(feature = "gcp")]
pub mod gcp;

#[cfg(feature = "gcp")]
pub mod gcp_auth;

#[cfg(feature = "gcp")]
pub mod gcp_compute;

#[cfg(feature = "e2b")]
pub mod e2b;

#[cfg(feature = "managed")]
pub mod managed;

pub use connect::connect_box;
pub use deployment::{Deployment, DeploymentConfig, merge_yaml};
pub use handle::{BoxHandle, BoxStatus, PowerState};
pub use provider::{
    CommandFuture, CommandRunner, SandboxError, SandboxProvider, SystemRunner, Tunnel,
    validate_handle_provider, validate_spec_provider,
};
pub use spec::SandboxSpec;
