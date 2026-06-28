//! Ingress reconciler — multi-domain routing with cert-manager TLS.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use std::collections::BTreeMap;
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{Ingress as IngressCR, IngressKindSpec};

use super::owner_ref_for;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<IngressCR>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("Ingress has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "Ingress");
    reconcile_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;
    Ok(Action::requeue(Duration::from_secs(60)))
}

async fn reconcile_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &IngressKindSpec,
    owner: OwnerReference,
) -> Result<()> {
    let class = if spec.ingress_class_name.is_empty() {
        "ingress"
    } else {
        &spec.ingress_class_name
    };
    let issuer = if spec.cluster_issuer.is_empty() {
        "letsencrypt-prod"
    } else {
        &spec.cluster_issuer
    };

    for (idx, domain) in spec.domains.iter().enumerate() {
        let ing_name = format!("{}-{}-{}", name, idx, sanitize_label(&domain.domain));
        let mut annotations: BTreeMap<String, String> = BTreeMap::new();
        annotations.insert("kubernetes.io/ingress.class".to_string(), class.to_string());
        if domain.tls {
            annotations.insert(
                "cert-manager.io/cluster-issuer".to_string(),
                issuer.to_string(),
            );
        }
        if let Some(extra) = &spec.annotations {
            for (k, v) in extra {
                annotations.insert(k.clone(), v.clone());
            }
        }

        let path_type = "Prefix".to_string();
        let paths: Vec<HTTPIngressPath> = domain
            .routes
            .iter()
            .map(|r| HTTPIngressPath {
                path: Some(r.path.clone()),
                path_type: if r.path_type.is_empty() {
                    path_type.clone()
                } else {
                    r.path_type.clone()
                },
                backend: IngressBackend {
                    service: Some(IngressServiceBackend {
                        name: r.service_name.clone(),
                        port: Some(ServiceBackendPort {
                            number: Some(r.service_port),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
            })
            .collect();

        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "hanzo-operator".to_string(),
        );
        if let Some(extra) = &spec.labels {
            for (k, v) in extra {
                labels.insert(k.clone(), v.clone());
            }
        }

        let tls = if domain.tls {
            Some(vec![IngressTLS {
                hosts: Some(vec![domain.domain.clone()]),
                secret_name: Some(format!("{}-tls", ing_name)),
            }])
        } else {
            None
        };

        let ing = Ingress {
            metadata: ObjectMeta {
                name: Some(ing_name.clone()),
                namespace: Some(namespace.to_string()),
                labels: Some(labels),
                annotations: Some(annotations),
                owner_references: Some(vec![owner.clone()]),
                ..Default::default()
            },
            spec: Some(K8sIngressSpec {
                // Annotation-only (set above): NEVER spec.ingressClassName — the
                // hanzoai/ingress (Traefik fork) drops the route when the spec
                // form is set. See manifests::build_ingress for the 2026-06-28
                // live re-confirmation (base/maxpower/superbase 404'd).
                rules: Some(vec![IngressRule {
                    host: Some(domain.domain.clone()),
                    http: Some(HTTPIngressRuleValue { paths }),
                }]),
                tls,
                ..Default::default()
            }),
            ..Default::default()
        };
        let ings: Api<Ingress> = Api::namespaced(client.clone(), namespace);
        apply::apply(&ings, &ing).await?;
    }

    info!(
        name,
        namespace,
        domains = spec.domains.len(),
        "Ingress reconciled"
    );
    Ok(())
}

fn sanitize_label(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn on_error(_obj: Arc<IngressCR>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "Ingress reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_ingress_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<IngressCR> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting Ingress controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "Ingress reconcile error");
            }
        })
        .await;
}
