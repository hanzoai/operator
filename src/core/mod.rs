//! Shared reconciler primitives for the Hanzo operator family.
//!
//! Absorbed from `hanzoai/operator-core` on the Go → Rust port, and this is the
//! canonical home for THIS operator — the copy here has since gained
//! `agents_client`, `health` and `visor_client`, so the two have diverged.
//!
//! operator-core itself is not retired, whatever an earlier note here said:
//! `zooai/operator` still depends on it (pinned at v0.1.0, while the crate is
//! at v0.2.0) and is deployed to zoo-mainnet, zoo-testnet, zoo-devnet and
//! zoo-system. So the primitives exist twice, and will until Zoo's four
//! `zoo.network` Kinds — ZooNetwork, ZooChain, ZooExplorer, ZooGateway — move
//! onto the shared ones. They are the same shapes the fleet already has, with
//! the brand welded into both the group and the Kind name; under the Family
//! split the chain three belong at `bootno.de` and Gateway at `zoo.cloud`,
//! which is what serving Zoo from this operator would mean. That is a
//! migration of live CRs, not a refactor.
//!
//! ## Modules
//!
//! | Module         | What it owns                                                            |
//! |----------------|-------------------------------------------------------------------------|
//! | [`error`]      | `OperatorError` — single canonical error type for the operator.         |
//! | [`leader`]     | `LeaderElection` — `coordination.k8s.io/v1` lease loop.                 |
//! | [`iam_admin`]  | IAM admin client (`POST /v1/iam/admin/applications/upsert`).            |
//! | [`agents_client`]| Cloud Agent registry client (`GET/POST /v1/agents`).                  |
//! | [`visor_client`]| Visor machine + agent-binding client (`/v1/machines`).                |
//! | [`secret`]     | Strict hijack guard + `\0` rejection for KMS-projected K8s Secrets.     |
//! | [`status`]     | Standard `status.conditions` mint helpers.                              |
//! | [`reconciler`] | `Action` requeue cadence + `clamp_resync`.                             |

pub mod agents_client;
pub mod error;
pub mod health;
pub mod iam_admin;
pub mod leader;
pub mod reconciler;
pub mod secret;
pub mod status;
pub mod visor_client;

pub use error::{OperatorError, Result};
pub use leader::{LeaderConfig, LeaderElection};
