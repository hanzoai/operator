//! Node GC — a Node whose EC2 instance is gone is deleted.
//!
//! k3s here runs no cloud controller, so nothing notices when an autoscaling
//! group terminates an instance. Its Node object lingers `NotReady`, and a
//! StatefulSet pod that was on it stays bound to a node that will never return
//! until a person deletes the Node (five were, by hand, on 2026-10-08). Deleting
//! the Node is the whole fix: kube-controller-manager's pod GC then removes the
//! pods bound to a node that no longer exists, and their controllers reschedule
//! them.
//!
//! A Node is deleted only when all four hold:
//!
//! 1. it is not Ready (`False` or `Unknown`);
//! 2. it has been not Ready for [`GRACE`];
//! 3. its providerID names an EC2 instance, `aws:///<zone>/<id>` — the k3s
//!    servers carry `k3s://…` and are never candidates;
//! 4. EC2 says that instance is `terminated`, or no longer knows it (AWS forgets
//!    a terminated instance after about an hour).
//!
//! The grace is for the second kind of answer. `InvalidInstanceID.NotFound` is
//! also what EC2 says, for a moment, of an instance launched a second ago (its
//! records are eventually consistent), and a Node that has just registered has no
//! Ready condition yet. Neither is NotReady for [`GRACE`], so neither is asked
//! about. `terminated` is final: an instance id is never reused.
//!
//! Any other answer, and any failure to get one, deletes nothing and asks again
//! in a minute. From the instance's death to its Node's deletion: about a minute
//! for the controller-manager to mark the kubelet silent, then [`GRACE`], then
//! at most one [`RECHECK`] for an instance still `shutting-down`. Under five
//! minutes, and only a cluster shows it (`LLM.md`, Node GC).
//!
//! ## Gate
//!
//! The controller runs when `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` are
//! in the environment (`core::ec2`), and not otherwise; with only one of them set
//! it says so at `warn`. The key's policy is `ec2:DescribeInstances`, and it must
//! be a key of the account whose instances the Nodes are (a key of another
//! account is told every instance is unknown); the operator's ClusterRole needs
//! `get`/`list`/`watch`/`delete` on `nodes`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, DeleteParams, Preconditions};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::{Client, ResourceExt};
use tracing::{debug, info, warn};

use crate::core::ec2::{Credentials, Ec2, Instance};
use crate::core::{OperatorError, Result};

/// How long a NotReady Node waits for its next look when its instance is not yet
/// terminated: an instance leaves `shutting-down` for `terminated` within
/// seconds to minutes, and a Node is deleted within a minute of that.
const RECHECK: Duration = Duration::from_secs(60);

/// How long a Node must have been not Ready before EC2 is asked about it.
const GRACE: Duration = Duration::from_secs(120);

pub struct Ctx {
    client: Client,
    ec2: Ec2,
}

/// Whether the kubelet reports the Node Ready. A Node with no Ready condition is
/// not.
fn ready(node: &Node) -> bool {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// When the Node last stopped being Ready: its Ready condition's transition, else
/// (a Node that has registered and reported nothing) its creation.
fn since(node: &Node) -> Option<jiff::Timestamp> {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
        .and_then(|c| c.last_transition_time.as_ref())
        .or(node.metadata.creation_timestamp.as_ref())
        .map(|t| t.0)
}

/// How much of [`GRACE`] is left at `now`. A Node of no known age, or one stamped
/// in the future (a skewed clock), has all of it.
fn remaining(node: &Node, now: jiff::Timestamp) -> Duration {
    let waited = since(node)
        .and_then(|t| Duration::try_from(now.duration_since(t)).ok())
        .unwrap_or_default();
    GRACE.saturating_sub(waited)
}

/// The EC2 instance a Node runs on, if its providerID names one.
fn instance_of(node: &Node) -> Option<Instance> {
    Instance::from_provider_id(node.spec.as_ref()?.provider_id.as_deref()?)
}

pub async fn reconcile(node: Arc<Node>, ctx: Arc<Ctx>) -> Result<Action> {
    // A Ready Node, a Node already being deleted, and a Node that is no EC2
    // instance (the k3s servers) are not this controller's. A Ready Node turning
    // NotReady is a change to the Node, which reconciles it again.
    if node.metadata.deletion_timestamp.is_some() || ready(&node) {
        return Ok(Action::await_change());
    }
    let Some(instance) = instance_of(&node) else {
        debug!(node = %node.name_any(), "NotReady Node is no EC2 instance; leaving it");
        return Ok(Action::await_change());
    };
    let left = remaining(&node, jiff::Timestamp::now());
    if !left.is_zero() {
        return Ok(Action::requeue(left));
    }
    if !ctx.ec2.gone(&instance).await? {
        return Ok(Action::requeue(RECHECK));
    }

    // Delete the Node as it was read: a Node that is not the one whose instance
    // is gone (deleted and registered again under the name) is refused.
    let name = node.name_any();
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: node.uid(),
            resource_version: None,
        }),
        ..Default::default()
    };
    match Api::<Node>::all(ctx.client.clone())
        .delete(&name, &params)
        .await
    {
        Ok(_) => {}
        // Deleted already, by a person or a second pass.
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    info!(
        node = %name,
        instance = instance.id(),
        region = instance.region(),
        "deleted Node: its EC2 instance is terminated"
    );
    Ok(Action::await_change())
}

pub fn on_error(node: Arc<Node>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    warn!(node = %node.name_any(), error = %err, "Node GC could not settle this Node; nothing deleted");
    Action::requeue(RECHECK)
}

/// The key Node GC runs on, or `None` and why (logged). No key at all is a
/// release that was not given one (`info`); half a key is a Secret mapped in part
/// (`warn`).
fn key(read: std::result::Result<Option<Credentials>, &'static str>) -> Option<Credentials> {
    match read {
        Ok(Some(credentials)) => Some(credentials),
        Ok(None) => {
            info!("Node GC disabled: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY not set");
            None
        }
        Err(missing) => {
            warn!(missing, "Node GC disabled: the AWS key is set in part");
            None
        }
    }
}

/// Watch Nodes and delete those whose EC2 instance is terminated. Does nothing
/// without AWS credentials in the environment.
pub async fn run_node_controller(client: Client) {
    if let Some(credentials) = key(Credentials::from_env()) {
        run(client, Ec2::new(credentials)).await
    }
}

async fn run(client: Client, ec2: Ec2) {
    info!("Starting Node GC (deletes Nodes whose EC2 instance is terminated)");
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        ec2,
    });
    Controller::new(Api::<Node>::all(client), WatcherConfig::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{delete, get, post};
    use axum::{Json, Router};
    use k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;

    const ZONE: &str = "us-east-1b";

    /// `ago` before now.
    fn back(ago: Duration) -> Time {
        Time(
            jiff::Timestamp::now()
                .checked_sub(jiff::SignedDuration::try_from(ago).unwrap())
                .unwrap(),
        )
    }

    /// `node`, its Ready condition last changed at `when`.
    fn since_then(mut node: Node, when: Option<Time>) -> Node {
        for c in node.status.as_mut().unwrap().conditions.as_mut().unwrap() {
            if c.type_ == "Ready" {
                c.last_transition_time = when.clone();
            }
        }
        node
    }

    /// `node`, not Ready for `ago`.
    fn not_ready_for(node: Node, ago: Duration) -> Node {
        since_then(node, Some(back(ago)))
    }

    /// A Node that has been in `ready` since long before [`GRACE`].
    fn node(name: &str, ready: &str, provider: Option<&str>) -> Node {
        Node {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                uid: Some(format!("uid-{name}")),
                creation_timestamp: Some(back(Duration::from_secs(86_400))),
                ..Default::default()
            },
            spec: Some(NodeSpec {
                provider_id: provider.map(str::to_string),
                ..Default::default()
            }),
            status: Some(NodeStatus {
                conditions: Some(vec![
                    NodeCondition {
                        type_: "DiskPressure".to_string(),
                        status: "False".to_string(),
                        ..Default::default()
                    },
                    NodeCondition {
                        type_: "Ready".to_string(),
                        status: ready.to_string(),
                        last_transition_time: Some(back(Duration::from_secs(3_600))),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
        }
    }

    /// A Node as the cluster holds an autoscaled worker: named for its instance.
    fn worker(id: &str, ready: &str) -> Node {
        node(id, ready, Some(&format!("aws:///{ZONE}/{id}")))
    }

    // ---- what the controller says ------------------------------------------

    /// What `tracing` printed, for the thread that holds the guard. A test runs on
    /// a current-thread runtime, so everything it awaits logs to this thread.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
        type Writer = Log;

        fn make_writer(&'a self) -> Log {
            self.clone()
        }
    }

    impl Log {
        fn capture() -> (Log, tracing::subscriber::DefaultGuard) {
            let log = Log::default();
            let guard = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(log.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::DEBUG)
                    .finish(),
            );
            (log, guard)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    // ---- what a Node is ----------------------------------------------------

    #[test]
    fn only_a_true_ready_condition_is_ready() {
        assert!(ready(&worker("i-0a", "True")));
        assert!(!ready(&worker("i-0a", "False")));
        assert!(!ready(&worker("i-0a", "Unknown")));
        let mut bare = worker("i-0a", "True");
        bare.status = None;
        assert!(!ready(&bare));
        bare.status = Some(NodeStatus::default());
        assert!(!ready(&bare));
    }

    #[test]
    fn a_worker_is_an_instance_and_a_k3s_server_is_not() {
        let i = instance_of(&worker("i-09ef3d3c8344724a2", "Unknown")).unwrap();
        assert_eq!(i.id(), "i-09ef3d3c8344724a2");
        assert_eq!(i.region(), "us-east-1");
        assert_eq!(
            instance_of(&node("ip-10-64-1-10", "False", Some("k3s://ip-10-64-1-10"))),
            None
        );
        assert_eq!(instance_of(&node("n", "False", None)), None);
        let mut no_spec = node("n", "False", None);
        no_spec.spec = None;
        assert_eq!(instance_of(&no_spec), None);
    }

    // ---- the cluster and AWS, as stand-ins ---------------------------------

    #[derive(Default)]
    struct World {
        /// Instance id → what EC2 answers for it: `(status, body)`.
        ec2: Mutex<HashMap<String, (u16, String)>>,
        /// Instance ids EC2 was asked about.
        asked: Mutex<Vec<String>>,
        /// `(name, DeleteOptions body)` of every Node DELETE.
        deleted: Mutex<Vec<(String, Value)>>,
        /// Nodes `list` returns until deleted.
        nodes: Mutex<Vec<Node>>,
        /// Status the apiserver answers a Node DELETE with.
        delete_status: Mutex<Option<u16>>,
    }

    fn described(id: &str, state: &str) -> (u16, String) {
        (
            200,
            format!(
                "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
                 <instanceId>{id}</instanceId><instanceState><code>0</code><name>{state}</name>\
                 </instanceState></item></instancesSet></item></reservationSet>\
                 </DescribeInstancesResponse>"
            ),
        )
    }

    fn refused(status: u16, code: &str) -> (u16, String) {
        (
            status,
            format!("<Response><Errors><Error><Code>{code}</Code></Error></Errors></Response>"),
        )
    }

    async fn ec2_answer(State(w): State<Arc<World>>, body: String) -> Response {
        let id = body
            .split('&')
            .find_map(|kv| kv.strip_prefix("InstanceId.1="))
            .unwrap_or("")
            .to_string();
        w.asked.lock().unwrap().push(id.clone());
        let (status, xml) = w
            .ec2
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_else(|| refused(400, "InvalidInstanceID.NotFound"));
        (StatusCode::from_u16(status).unwrap(), xml).into_response()
    }

    async fn node_delete(
        State(w): State<Arc<World>>,
        Path(name): Path<String>,
        body: axum::body::Bytes,
    ) -> Response {
        let options: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        w.deleted.lock().unwrap().push((name.clone(), options));
        let status = w.delete_status.lock().unwrap().unwrap_or(200);
        if status == 200 {
            w.nodes
                .lock()
                .unwrap()
                .retain(|n| n.metadata.name.as_deref() != Some(&name));
        }
        (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            json!({"kind": "Status", "apiVersion": "v1",
                   "status": if status == 200 { "Success" } else { "Failure" },
                   "message": "stand-in", "reason": "StandIn", "code": status})
            .to_string(),
        )
            .into_response()
    }

    /// `list` and `watch` of Nodes: the list is the world's Nodes, and a watch
    /// ends at once, which the watcher answers by listing again.
    async fn node_list(
        State(w): State<Arc<World>>,
        Query(q): Query<BTreeMap<String, String>>,
    ) -> Response {
        if q.get("watch").is_some_and(|v| v == "true" || v == "1") {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return (StatusCode::OK, "").into_response();
        }
        let items = w.nodes.lock().unwrap().clone();
        Json(json!({
            "kind": "NodeList", "apiVersion": "v1",
            "metadata": {"resourceVersion": "1"},
            "items": items,
        }))
        .into_response()
    }

    /// Serve the world on an ephemeral port: the apiserver paths and an EC2
    /// endpoint. Returns the base URL.
    async fn serve(w: Arc<World>) -> String {
        let app = Router::new()
            .route("/api/v1/nodes", get(node_list))
            .route("/api/v1/nodes/{name}", delete(node_delete))
            .route("/", post(ec2_answer))
            .with_state(w);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        format!("http://{addr}")
    }

    fn ctx(url: &str) -> Arc<Ctx> {
        // The install `main` makes before any TLS client is built; a lib test
        // never runs `main`.
        static PROVIDER: std::sync::Once = std::sync::Once::new();
        PROVIDER.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        Arc::new(Ctx {
            client: Client::try_from(kube::Config::new(url.parse().unwrap())).unwrap(),
            ec2: Ec2::at(
                Credentials::new("AKIDEXAMPLE", "secret", None),
                format!("{url}/").parse().unwrap(),
            ),
        })
    }

    fn world(answers: &[(&str, (u16, String))]) -> Arc<World> {
        let w = Arc::new(World::default());
        for (id, answer) in answers {
            w.ec2.lock().unwrap().insert(id.to_string(), answer.clone());
        }
        w
    }

    fn deleted(w: &World) -> Vec<String> {
        w.deleted
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.0.clone())
            .collect()
    }

    fn asked(w: &World) -> Vec<String> {
        w.asked.lock().unwrap().clone()
    }

    // ---- reconcile ---------------------------------------------------------

    #[tokio::test]
    async fn a_dead_instances_node_is_deleted_as_read() {
        let (log, _guard) = Log::capture();
        let w = world(&[("i-0a1d", described("i-0a1d", "terminated"))]);
        let ctx = ctx(&serve(w.clone()).await);

        let action = reconcile(Arc::new(worker("i-0a1d", "Unknown")), ctx)
            .await
            .unwrap();

        assert_eq!(action, Action::await_change());
        assert_eq!(deleted(&w), ["i-0a1d"]);
        // The delete names the Node it read, so a Node registered again under the
        // name is not the one deleted.
        let options = w.deleted.lock().unwrap()[0].1.clone();
        assert_eq!(options["preconditions"]["uid"], "uid-i-0a1d");

        // The deletion is logged, with the Node, its instance and its region.
        let text = log.text();
        assert!(text.contains(" INFO "), "{text}");
        assert!(
            text.contains("deleted Node: its EC2 instance is terminated"),
            "{text}"
        );
        for fact in ["i-0a1d", "us-east-1"] {
            assert!(text.contains(fact), "{fact} in {text}");
        }
    }

    #[tokio::test]
    async fn a_node_ec2_has_forgotten_is_deleted() {
        // No answer registered: the stand-in says what EC2 says of an instance
        // it no longer holds.
        let w = world(&[]);
        let ctx = ctx(&serve(w.clone()).await);
        reconcile(Arc::new(worker("i-0b2e", "False")), ctx)
            .await
            .unwrap();
        assert_eq!(deleted(&w), ["i-0b2e"]);
    }

    #[tokio::test]
    async fn a_node_whose_instance_lives_is_kept_and_looked_at_again() {
        for state in ["pending", "running", "shutting-down", "stopping", "stopped"] {
            let w = world(&[("i-0c3f", described("i-0c3f", state))]);
            let ctx = ctx(&serve(w.clone()).await);

            let action = reconcile(Arc::new(worker("i-0c3f", "Unknown")), ctx)
                .await
                .unwrap();

            assert_eq!(action, Action::requeue(RECHECK), "{state}");
            assert_eq!(asked(&w), ["i-0c3f"], "{state}");
            assert!(deleted(&w).is_empty(), "{state}");
        }
    }

    #[tokio::test]
    async fn a_ready_node_costs_no_call_and_no_delete() {
        let w = world(&[("i-0d4a", described("i-0d4a", "terminated"))]);
        let ctx = ctx(&serve(w.clone()).await);
        let action = reconcile(Arc::new(worker("i-0d4a", "True")), ctx)
            .await
            .unwrap();
        assert_eq!(action, Action::await_change());
        assert!(asked(&w).is_empty());
        assert!(deleted(&w).is_empty());
    }

    #[tokio::test]
    async fn a_k3s_server_is_neither_asked_about_nor_deleted() {
        let w = world(&[]);
        let ctx = ctx(&serve(w.clone()).await);
        let server = node("ip-10-64-1-10", "Unknown", Some("k3s://ip-10-64-1-10"));
        let action = reconcile(Arc::new(server), ctx).await.unwrap();
        assert_eq!(action, Action::await_change());
        assert!(asked(&w).is_empty());
        assert!(deleted(&w).is_empty());
    }

    #[tokio::test]
    async fn a_node_already_being_deleted_is_left() {
        let w = world(&[]);
        let ctx = ctx(&serve(w.clone()).await);
        let mut n = worker("i-0e5b", "Unknown");
        n.metadata.deletion_timestamp = Some(Time(jiff::Timestamp::now()));
        reconcile(Arc::new(n), ctx).await.unwrap();
        assert!(asked(&w).is_empty());
        assert!(deleted(&w).is_empty());
    }

    /// Anything but AWS's word that the instance is gone deletes nothing: a
    /// refusal, throttling, a server error, an answer that is not EC2's.
    #[tokio::test]
    async fn no_verdict_from_aws_deletes_nothing() {
        for (id, answer) in [
            ("i-0f6c", refused(403, "UnauthorizedOperation")),
            ("i-0f6d", refused(401, "AuthFailure")),
            ("i-0f6e", refused(503, "RequestLimitExceeded")),
            ("i-0f60", (500, String::new())),
            ("i-0f61", (200, "<html>captive portal</html>".to_string())),
            ("i-0f62", described("i-0f69", "terminated")),
        ] {
            let w = world(&[(id, answer)]);
            let ctx = ctx(&serve(w.clone()).await);
            let n = Arc::new(worker(id, "Unknown"));

            let outcome = reconcile(n.clone(), ctx.clone()).await;

            assert!(outcome.is_err(), "{id}");
            assert!(deleted(&w).is_empty(), "{id}");
            // The error is retried on the same cadence as a living instance.
            let err = outcome.unwrap_err();
            assert_eq!(on_error(n, &err, ctx), Action::requeue(RECHECK), "{id}");
        }
    }

    #[tokio::test]
    async fn a_node_deleted_by_someone_else_is_not_an_error() {
        let w = world(&[("i-0a7d", described("i-0a7d", "terminated"))]);
        *w.delete_status.lock().unwrap() = Some(404);
        let ctx = ctx(&serve(w.clone()).await);
        let action = reconcile(Arc::new(worker("i-0a7d", "Unknown")), ctx)
            .await
            .unwrap();
        assert_eq!(action, Action::await_change());
    }

    #[tokio::test]
    async fn a_refused_delete_is_an_error_to_retry() {
        let w = world(&[("i-0a8e", described("i-0a8e", "terminated"))]);
        *w.delete_status.lock().unwrap() = Some(409);
        let ctx = ctx(&serve(w.clone()).await);
        let outcome = reconcile(Arc::new(worker("i-0a8e", "Unknown")), ctx).await;
        assert!(outcome.is_err());
    }

    // ---- the grace -----------------------------------------------------------

    #[test]
    fn the_grace_runs_from_the_last_transition() {
        let now = jiff::Timestamp::now();
        let at = |secs: i64| {
            Time(
                now.checked_sub(jiff::SignedDuration::from_secs(secs))
                    .unwrap(),
            )
        };
        let n = worker("i-0f70", "Unknown");
        let left = |when: Option<Time>| remaining(&since_then(n.clone(), when), now);

        assert_eq!(left(Some(at(0))), GRACE);
        assert_eq!(left(Some(at(30))), GRACE - Duration::from_secs(30));
        assert_eq!(left(Some(at(GRACE.as_secs() as i64))), Duration::ZERO);
        assert_eq!(left(Some(at(3_600))), Duration::ZERO);
        // A transition stamped after now (a clock that runs behind) is no age.
        assert_eq!(left(Some(at(-30))), GRACE);

        // No transition time: the Node's creation is when it began.
        let mut created = since_then(n.clone(), None);
        created.metadata.creation_timestamp = Some(at(30));
        assert_eq!(remaining(&created, now), GRACE - Duration::from_secs(30));

        // No time at all: the whole grace, so a Node of no known age is not asked
        // about.
        created.metadata.creation_timestamp = None;
        assert_eq!(remaining(&created, now), GRACE);
    }

    /// EC2 says `NotFound` of an instance launched a moment ago, and a terminated
    /// instance is final, but neither is asked about before the Node has been
    /// not Ready for the grace.
    #[tokio::test]
    async fn a_node_just_not_ready_is_not_asked_about_yet() {
        for (what, answer) in [
            ("EC2 has not heard of it", None),
            (
                "EC2 says terminated",
                Some(described("i-0f71", "terminated")),
            ),
        ] {
            let answers: Vec<_> = answer.map(|a| ("i-0f71", a)).into_iter().collect();
            let w = world(&answers);
            let ctx = ctx(&serve(w.clone()).await);
            let young = not_ready_for(worker("i-0f71", "Unknown"), Duration::from_secs(5));

            let action = reconcile(Arc::new(young), ctx).await.unwrap();

            assert_ne!(action, Action::await_change(), "{what}");
            assert_ne!(action, Action::requeue(RECHECK), "{what}");
            assert!(asked(&w).is_empty(), "{what}");
            assert!(deleted(&w).is_empty(), "{what}");
        }
    }

    #[tokio::test]
    async fn a_node_that_never_reported_is_judged_by_its_creation() {
        let bare = |created: Duration| {
            let mut n = worker("i-0f72", "Unknown");
            n.status = None;
            n.metadata.creation_timestamp = Some(back(created));
            n
        };

        // Registered a moment ago: nothing asked, nothing deleted.
        let w = world(&[]);
        let ctx_ = ctx(&serve(w.clone()).await);
        reconcile(Arc::new(bare(Duration::from_secs(5))), ctx_)
            .await
            .unwrap();
        assert!(asked(&w).is_empty());
        assert!(deleted(&w).is_empty());

        // Registered long ago and never Ready, its instance forgotten: deleted.
        let w = world(&[]);
        let ctx_ = ctx(&serve(w.clone()).await);
        reconcile(Arc::new(bare(Duration::from_secs(600))), ctx_)
            .await
            .unwrap();
        assert_eq!(deleted(&w), ["i-0f72"]);
    }

    #[tokio::test]
    async fn a_node_not_ready_past_the_grace_is_asked_about() {
        let w = world(&[("i-0f73", described("i-0f73", "terminated"))]);
        let ctx = ctx(&serve(w.clone()).await);
        let n = not_ready_for(worker("i-0f73", "Unknown"), GRACE + Duration::from_secs(1));
        reconcile(Arc::new(n), ctx).await.unwrap();
        assert_eq!(deleted(&w), ["i-0f73"]);
    }

    // ---- the gate ------------------------------------------------------------

    #[test]
    fn the_key_gates_the_controller_and_says_why_it_does_not_run() {
        let (log, _guard) = Log::capture();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            }
        };

        let both = &[
            ("AWS_ACCESS_KEY_ID", "KEYID-0001"),
            ("AWS_SECRET_ACCESS_KEY", "SECRETVALUE-0002"),
        ];
        assert!(key(Credentials::read(env(both))).is_some());
        assert!(log.text().is_empty(), "{}", log.text());

        assert!(key(Credentials::read(env(&[]))).is_none());
        let text = log.text();
        assert!(
            text.contains(" INFO ") && text.contains("Node GC disabled"),
            "{text}"
        );
        assert!(!text.contains(" WARN "), "{text}");

        let half = &[("AWS_ACCESS_KEY_ID", "KEYID-0001")];
        assert!(key(Credentials::read(env(half))).is_none());
        let text = log.text();
        assert!(
            text.contains(" WARN ") && text.contains("set in part"),
            "{text}"
        );
        assert!(text.contains("AWS_SECRET_ACCESS_KEY"), "{text}");
        // The key itself is in no line.
        assert!(
            !text.contains("KEYID-0001") && !text.contains("SECRETVALUE"),
            "{text}"
        );
    }

    // ---- the controller ----------------------------------------------------

    /// The shape of 2026-10-08, run through the controller itself: a cluster
    /// holding a dead spot node among live nodes and the k3s servers. The dead
    /// node's Node is deleted, with nothing else touched and nothing else asked
    /// of AWS.
    #[tokio::test]
    async fn the_controller_deletes_the_dead_node_and_only_that() {
        let w = world(&[
            ("i-0b1a", described("i-0b1a", "running")),
            ("i-0b2b", described("i-0b2b", "terminated")),
            ("i-0b3c", described("i-0b3c", "running")),
        ]);
        *w.nodes.lock().unwrap() = vec![
            worker("i-0b1a", "True"),
            worker("i-0b2b", "Unknown"),
            worker("i-0b3c", "Unknown"),
            node("ip-10-64-1-10", "True", Some("k3s://ip-10-64-1-10")),
            node("ip-10-64-1-20", "Unknown", Some("k3s://ip-10-64-1-20")),
        ];
        let url = serve(w.clone()).await;
        let ctx = ctx(&url);
        let run = tokio::spawn(run(
            ctx.client.clone(),
            Ec2::at(
                Credentials::new("AKIDEXAMPLE", "secret", None),
                format!("{url}/").parse().unwrap(),
            ),
        ));

        let started = std::time::Instant::now();
        while deleted(&w).is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "no Node deleted"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Give the watch a few more rounds to do anything else it would.
        tokio::time::sleep(Duration::from_millis(800)).await;
        run.abort();

        assert_eq!(deleted(&w), ["i-0b2b"]);
        let mut seen = asked(&w);
        seen.sort();
        seen.dedup();
        assert_eq!(seen, ["i-0b2b", "i-0b3c"]);
        let left: Vec<_> = w
            .nodes
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.name_any())
            .collect();
        assert_eq!(left, ["i-0b1a", "i-0b3c", "ip-10-64-1-10", "ip-10-64-1-20"]);
    }
}
