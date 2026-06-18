//! HanzoDNS reconciler — backcompat alias for the canonical DNS Kind.
//! Newtype facade over DNSSpec; delegates to the SAME inner handler as DNS
//! (`dns::reconcile_dns_inner_pub`). No duplicated reconcile.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::HanzoDNS;

use super::{dns, owner_ref_for};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<HanzoDNS>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("HanzoDNS has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "HanzoDNS");
    dns::reconcile_dns_inner_pub(&ctx.client, &name, &namespace, &cr.spec.0, owner).await?;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<HanzoDNS>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "HanzoDNS reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_hanzo_dns_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<HanzoDNS> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting HanzoDNS controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::crd::{DNSSpec, DNSZoneSpec, HanzoDNSSpec};

    #[test]
    fn hanzo_dns_spec_unwraps_to_dns_spec() {
        let inner = DNSSpec {
            zones: vec![DNSZoneSpec {
                name: "hanzo.ai".to_string(),
                cloudflare_zone_id: String::new(),
            }],
            coredns: None,
            cloudflare: None,
            database: None,
            oidc: None,
            ingress: None,
            labels: None,
            annotations: None,
        };
        let facade = HanzoDNSSpec(inner.clone());
        assert_eq!(facade.0.zones.len(), 1);
        assert_eq!(facade.0.zones[0].name, "hanzo.ai");
    }
}
