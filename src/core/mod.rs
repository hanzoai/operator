//! Shared reconciler primitives for the Hanzo operator family.
//!
//! Absorbed from `~/work/hanzo/operator-core` on the Go → Rust port. This
//! crate is now the canonical home; the standalone operator-core repo is a
//! tombstone pointing here.
//!
//! ## Modules
//!
//! | Module        | What it owns                                                            |
//! |---------------|-------------------------------------------------------------------------|
//! | [`error`]     | `OperatorError` — single canonical error type for the operator.         |
//! | [`leader`]    | `LeaderElection` — `coordination.k8s.io/v1` lease loop.                 |
//! | [`iam_admin`] | IAM admin client (`POST /v1/iam/admin/applications/upsert`).            |
//! | [`secret`]    | Strict hijack guard + `\0` rejection for KMS-projected K8s Secrets.     |
//! | [`status`]    | Standard `status.conditions` mint helpers.                              |
//! | [`reconciler`]| `Action` requeue cadence + `clamp_resync`.                              |

pub mod error;
pub mod iam_admin;
pub mod leader;
pub mod reconciler;
pub mod secret;
pub mod status;

pub use error::{OperatorError, Result};
pub use leader::{LeaderConfig, LeaderElection};
