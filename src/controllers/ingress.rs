//! Ingress reconciler — multi-domain routing with cert-manager TLS.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, DeleteParams, ListParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use std::collections::{BTreeMap, BTreeSet};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{DomainConfig, Ingress as IngressCR, IngressKindSpec};

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

/// Public inner handler — shared entrypoint for the Ingress reconcile, so the
/// `App` role-dispatch facade (role=ingress) drives the SAME multi-domain
/// reconcile as a native `Ingress` CR, threaded with the App's owner reference
/// (owner-scoped prune included — see `prune_superseded`). Mirrors
/// `dns::reconcile_dns_inner_pub`.
pub async fn reconcile_ingress_inner_pub(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &IngressKindSpec,
    owner: OwnerReference,
) -> Result<()> {
    reconcile_inner(client, name, namespace, spec, owner).await
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

    let ings: Api<Ingress> = Api::namespaced(client.clone(), namespace);
    let mut desired: BTreeSet<String> = BTreeSet::new();
    for domain in spec.domains.iter() {
        let ing = build_domain_ingress(
            name,
            namespace,
            domain,
            class,
            issuer,
            spec.annotations.as_ref(),
            spec.labels.as_ref(),
            &owner,
        );
        // Names come from hosts now, so a host listed twice collides instead of
        // quietly producing two Ingresses for it — which was never routable
        // anyway, just harder to see. Say so and keep the last one.
        if !desired.insert(ing.name_any()) {
            warn!(
                name,
                namespace,
                domain = %domain.domain,
                "duplicate domain in spec.domains; later entry wins"
            );
        }
        apply::apply(&ings, &ing).await?;
    }

    let pruned = prune_superseded(&ings, &owner, &desired).await?;

    info!(
        name,
        namespace,
        domains = spec.domains.len(),
        pruned,
        "Ingress reconciled"
    );
    Ok(())
}

/// Delete the Ingress shards this CR previously wrote that the current
/// generation no longer declares. A shard becomes superseded when the host is
/// dropped from the CR (a backend cutover) or the child-name scheme changed
/// across operator versions — including the index-to-host rename, whose stale
/// `<parent>-<idx>-<host>` shards this sweep is what clears.  Moving a host
/// within `domains` is no longer a supersede cause: names are host-derived, so
/// order is not identity. In every case the stale shard keeps a valid `ownerReference` to
/// the still-live CR, so k8s garbage collection never fires — owner-ref GC only
/// triggers on CR *deletion*. Left alone, the stale shard stays live at equal
/// priority and keeps routing a cut-over host to its OLD backend.
///
/// Safety boundary — a shard is deleted only when ALL of these hold. It carries
/// `app.kubernetes.io/managed-by=hanzo-operator` (server-side selector), so the
/// operator never touches an Ingress it does not manage. One of its
/// `ownerReferences` UIDs equals THIS CR's UID, so the sweep is scoped to this
/// CR — never a sibling CR, a foreign object, or a host another CR owns. Its
/// name is NOT in `desired`, so the current generation is always kept and the
/// sweep is idempotent. Without a CR UID the sweep cannot scope safely and is a
/// no-op.
async fn prune_superseded(
    ings: &Api<Ingress>,
    owner: &OwnerReference,
    desired: &BTreeSet<String>,
) -> Result<usize> {
    if owner.uid.is_empty() {
        return Ok(0);
    }
    let live = ings
        .list(&ListParams::default().labels("app.kubernetes.io/managed-by=hanzo-operator"))
        .await?;
    let dp = DeleteParams::default();
    let mut pruned = 0usize;
    for ing in live {
        let ing_name = match ing.metadata.name.clone() {
            Some(n) => n,
            None => continue,
        };
        let doomed = {
            let owner_uids: Vec<&str> = ing
                .metadata
                .owner_references
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|r| r.uid.as_str())
                .collect();
            is_superseded(&ing_name, &owner_uids, &owner.uid, desired)
        };
        if !doomed {
            continue;
        }
        match ings.delete(&ing_name, &dp).await {
            Ok(_) => {
                info!(ingress = %ing_name, "pruned superseded Ingress shard");
                pruned += 1;
            }
            // Already gone (concurrent reconcile / manual cleanup): idempotent.
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => return Err(OperatorError::KubeApi(e)),
        }
    }
    Ok(pruned)
}

/// Pure prune decision, separated from the delete effect so the safety rules are
/// unit-tested without a kube client. Returns true iff the shard `name` (with
/// `owner_uids` drawn from its `ownerReferences`) is owned by the CR identified
/// by `cr_uid` yet absent from the current-generation `desired` set.
fn is_superseded(
    name: &str,
    owner_uids: &[&str],
    cr_uid: &str,
    desired: &BTreeSet<String>,
) -> bool {
    !cr_uid.is_empty() && !desired.contains(name) && owner_uids.contains(&cr_uid)
}

/// Materialize one k8s `Ingress` from a single `DomainConfig`. Pure (no cluster
/// I/O) so it is unit-tested without a kube client — the host, TLS host, issuer
/// annotation, and per-route backends are all asserted here rather than in prod.
///
/// A wildcard `domain` (`*.hanzo.app`, the published-sites edge) is a first-class
/// case: it becomes the k8s Ingress host verbatim (k8s wildcard hosts match
/// exactly one label — `foo.hanzo.app`, never the apex or `a.b.hanzo.app`) and
/// the TLS entry carries the same wildcard so cert-manager issues a `*.hanzo.app`
/// cert via the DNS-01 issuer. No reserved-host exclusion lives here: the backend
/// (cloud `clients/sites`) is the authoritative reserved-label guard.
#[allow(clippy::too_many_arguments)]
fn build_domain_ingress(
    parent_name: &str,
    namespace: &str,
    domain: &DomainConfig,
    class: &str,
    issuer: &str,
    extra_annotations: Option<&BTreeMap<String, String>>,
    extra_labels: Option<&BTreeMap<String, String>>,
    owner: &OwnerReference,
) -> Ingress {
    // Named for the host, NOT its position in `spec.domains`. The index used to
    // be in here, which made the name a function of list order: inserting one
    // domain renamed every Ingress after it, and each rename is a new name is a
    // new cert-manager Certificate is a new Let's Encrypt issuance. The old
    // Ingress got pruned and its Certificate garbage-collected with it, but
    // cert-manager deliberately never deletes the Secret — so each reorder left
    // a dead TLS Secret behind for every domain downstream of the edit. That is
    // how one namespace accumulated 141 orphaned Secrets and nine generations of
    // certs for the same four kms hosts, burning issuances against the
    // per-registered-domain rate limit for no reason at all.
    //
    // The host is already unique within a parent's domain list and is what the
    // Ingress actually serves, so it is the honest name. Reordering `domains` is
    // now a no-op.
    let ing_name = format!("{}-{}", parent_name, sanitize_label(&domain.domain));

    let mut annotations: BTreeMap<String, String> = BTreeMap::new();
    annotations.insert("kubernetes.io/ingress.class".to_string(), class.to_string());
    if domain.tls {
        annotations.insert(
            "cert-manager.io/cluster-issuer".to_string(),
            issuer.to_string(),
        );
    }
    if let Some(extra) = extra_annotations {
        for (k, v) in extra {
            annotations.insert(k.clone(), v.clone());
        }
    }
    // Per-domain annotations merge LAST, so a single host's edge-behavior
    // (its Traefik middleware chain, a redirect, an auth-guard) lands on ONLY
    // this domain's Ingress and wins over any CR-level default.
    if let Some(per_domain) = domain.annotations.as_ref() {
        for (k, v) in per_domain {
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
    if let Some(extra) = extra_labels {
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

    Ingress {
        metadata: ObjectMeta {
            name: Some(ing_name),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(K8sIngressSpec {
            rules: Some(vec![IngressRule {
                host: Some(domain.domain.clone()),
                http: Some(HTTPIngressRuleValue { paths }),
            }]),
            tls,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// k8s object-name-safe rendering of a domain. A wildcard `*` becomes the literal
/// `wildcard` (so `*.hanzo.app` → `wildcard-hanzo-app`, a valid RFC-1123 name)
/// instead of a leading-dash fragment; every other non-alphanumeric byte maps to
/// `-`. Non-wildcard domains are unaffected, so existing Ingress child names never
/// churn.
fn sanitize_label(s: &str) -> String {
    s.replace('*', "wildcard")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn on_error(_obj: Arc<IngressCR>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "Ingress reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::IngressRoute;

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "hanzo.ai/v1".to_string(),
            kind: "Ingress".to_string(),
            name: "hanzo-app-sites".to_string(),
            uid: "test-uid".to_string(),
            ..Default::default()
        }
    }

    fn cloud_route() -> IngressRoute {
        IngressRoute {
            path: "/".to_string(),
            path_type: "Prefix".to_string(),
            service_name: "cloud".to_string(),
            service_port: 8000,
        }
    }

    // The published-sites edge (`*.hanzo.app` → cloud:8000) is the exact CR the
    // universe declares (infra/k8s/operator/crs/hanzo-app-sites.yaml) after the
    // hand-edited routes.yaml router was reverted. These lock that the operator
    // materializes it natively.

    #[test]
    fn wildcard_domain_becomes_the_ingress_host_verbatim() {
        let domain = DomainConfig {
            domain: "*.hanzo.app".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let ing = build_domain_ingress(
            "hanzo-app-sites",
            "hanzo",
            &domain,
            "ingress",
            "letsencrypt-prod",
            None,
            None,
            &owner(),
        );
        let rules = ing.spec.unwrap().rules.expect("rules");
        assert_eq!(
            rules[0].host.as_deref(),
            Some("*.hanzo.app"),
            "the wildcard host must be carried verbatim so Traefik matches every single-label subdomain"
        );
    }

    #[test]
    fn per_domain_annotations_land_on_that_domain_and_win_over_cr_level() {
        // The edge-behavior seam: a host carries its own Traefik middleware chain
        // (redirect / auth-guard / rewrite) via a per-domain annotation, merged on
        // top of the CR-level annotations onto ONLY this domain's Ingress.
        let mut per_domain = BTreeMap::new();
        per_domain.insert(
            "traefik.ingress.kubernetes.io/router.middlewares".to_string(),
            "hanzo-oauth-authorize-to-login@kubernetescrd".to_string(),
        );
        // A CR-level default the per-domain entry must override for this host.
        per_domain.insert("hanzo.ai/edge".to_string(), "per-domain".to_string());
        let domain = DomainConfig {
            domain: "iam.hanzo.ai".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: Some(per_domain),
        };
        let mut cr_level = BTreeMap::new();
        cr_level.insert("hanzo.ai/edge".to_string(), "cr-level".to_string());
        let ing = build_domain_ingress(
            "hanzo-domains",
            "hanzo",
            &domain,
            "ingress",
            "letsencrypt-prod",
            Some(&cr_level),
            None,
            &owner(),
        );
        let ann = ing.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            ann.get("traefik.ingress.kubernetes.io/router.middlewares")
                .map(String::as_str),
            Some("hanzo-oauth-authorize-to-login@kubernetescrd"),
            "per-domain middleware annotation must land on this domain's Ingress"
        );
        assert_eq!(
            ann.get("hanzo.ai/edge").map(String::as_str),
            Some("per-domain"),
            "per-domain annotation must override the CR-level default"
        );
        assert_eq!(
            ann.get("kubernetes.io/ingress.class").map(String::as_str),
            Some("ingress")
        );
    }

    #[test]
    fn absent_per_domain_annotations_leave_cr_level_intact() {
        // A domain with no per-domain annotations keeps the CR-level ones verbatim,
        // so the field is purely additive (no behavior change for existing CRs).
        let domain = DomainConfig {
            domain: "cloud.hanzo.ai".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let mut cr_level = BTreeMap::new();
        cr_level.insert("hanzo.ai/edge".to_string(), "cr-level".to_string());
        let ing = build_domain_ingress(
            "hanzo-domains",
            "hanzo",
            &domain,
            "ingress",
            "letsencrypt-prod",
            Some(&cr_level),
            None,
            &owner(),
        );
        let ann = ing.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            ann.get("hanzo.ai/edge").map(String::as_str),
            Some("cr-level"),
            "CR-level annotation is untouched when no per-domain annotations are set"
        );
    }

    #[test]
    fn wildcard_domain_emits_wildcard_tls_and_issuer() {
        let domain = DomainConfig {
            domain: "*.hanzo.app".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let ing = build_domain_ingress(
            "hanzo-app-sites",
            "hanzo",
            &domain,
            "ingress",
            "letsencrypt-prod",
            None,
            None,
            &owner(),
        );
        // cert-manager issuer annotation present (DNS-01 issuer mints the *.hanzo.app cert).
        let ann = ing.metadata.annotations.as_ref().expect("annotations");
        assert_eq!(
            ann.get("cert-manager.io/cluster-issuer")
                .map(String::as_str),
            Some("letsencrypt-prod")
        );
        assert_eq!(
            ann.get("kubernetes.io/ingress.class").map(String::as_str),
            Some("ingress")
        );
        // TLS entry carries the wildcard host so the issued cert is *.hanzo.app.
        let tls = ing.spec.unwrap().tls.expect("tls");
        assert_eq!(
            tls[0].hosts.as_ref().unwrap(),
            &vec!["*.hanzo.app".to_string()]
        );
        // Object-name-safe secret name (no leading-dash fragment from the `*`).
        assert_eq!(
            tls[0].secret_name.as_deref(),
            Some("hanzo-app-sites-wildcard-hanzo-app-tls")
        );
    }

    #[test]
    fn wildcard_domain_routes_to_the_declared_backend() {
        let domain = DomainConfig {
            domain: "*.hanzo.app".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let ing = build_domain_ingress(
            "hanzo-app-sites",
            "hanzo",
            &domain,
            "ingress",
            "letsencrypt-prod",
            None,
            None,
            &owner(),
        );
        let rules = ing.spec.unwrap().rules.unwrap();
        let path = &rules[0].http.as_ref().unwrap().paths[0];
        assert_eq!(path.path.as_deref(), Some("/"));
        assert_eq!(path.path_type, "Prefix");
        let be = path.backend.service.as_ref().unwrap();
        assert_eq!(be.name, "cloud");
        assert_eq!(be.port.as_ref().unwrap().number, Some(8000));
        // Owner ref set so the child Ingress is GC'd with the CR.
        let refs = ing.metadata.owner_references.expect("owner refs");
        assert_eq!(refs[0].kind, "Ingress");
    }

    #[test]
    fn wildcard_child_name_is_object_safe() {
        // The `*` must not produce a leading-dash name fragment (invalid k8s name).
        assert_eq!(sanitize_label("*.hanzo.app"), "wildcard-hanzo-app");
    }

    #[test]
    fn plain_domain_name_is_unchanged() {
        // Non-wildcard domains must sanitize exactly as before, so existing
        // Ingress child names (hanzo-domains-*) never churn.
        assert_eq!(sanitize_label("api.cloud.hanzo.ai"), "api-cloud-hanzo-ai");
    }

    #[test]
    fn child_name_is_a_function_of_the_host_not_the_list_position() {
        // The property the old `<parent>-<idx>-<host>` scheme did not have.
        // Under it, inserting a domain at the front renamed every Ingress behind
        // it, and each rename bought a fresh cert-manager Certificate, a fresh
        // Let's Encrypt issuance, and one permanently orphaned TLS Secret (the
        // Ingress is pruned and its Certificate GC'd, but cert-manager never
        // deletes the Secret). Nine such generations accumulated for the four
        // `kms.*` hosts alone. Naming by host makes a reorder a no-op.
        let d = |host: &str| DomainConfig {
            domain: host.to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let build = |dom: &DomainConfig| {
            build_domain_ingress(
                "hanzo-domains",
                "hanzo",
                dom,
                "ingress",
                "letsencrypt-prod",
                None,
                None,
                &owner(),
            )
            .name_any()
        };

        let kms = d("kms.hanzo.ai");
        // Same host, whatever it is preceded by in `spec.domains`.
        assert_eq!(build(&kms), "hanzo-domains-kms-hanzo-ai");
        assert_eq!(build(&kms), build(&d("kms.hanzo.ai")));
        // Distinct hosts still get distinct names — collapsing them would be the
        // opposite failure.
        assert_ne!(build(&kms), build(&d("kms.lux.network")));
        // And no index survives anywhere in the name.
        assert!(!build(&kms).contains("-0-"));
    }

    #[test]
    fn tls_secret_name_tracks_the_host_too() {
        // The Secret name is derived from the Ingress name, so it inherits the
        // stability — this is the name whose churn actually cost issuances.
        let dom = DomainConfig {
            domain: "kms.hanzo.ai".to_string(),
            routes: vec![cloud_route()],
            tls: true,
            annotations: None,
        };
        let ing = build_domain_ingress(
            "hanzo-domains",
            "hanzo",
            &dom,
            "ingress",
            "letsencrypt-prod",
            None,
            None,
            &owner(),
        );
        let secret = ing.spec.unwrap().tls.unwrap()[0].secret_name.clone();
        assert_eq!(secret.as_deref(), Some("hanzo-domains-kms-hanzo-ai-tls"));
    }

    // Prune decision — the `platform.hanzo.ai` cutover leak and its boundaries.
    // The current generation writes the host-derived `hanzo-domains-platform-hanzo-ai`.
    // Every index-scheme shard it replaces (`-2-`, `-4-`, …) lingers with a valid
    // ownerReference to the still-live `hanzo-domains` CR (uid `cr-uid`), so this
    // sweep is what retires them — owner-ref GC never fires while the CR lives.

    const CR_UID: &str = "cr-uid";

    fn desired() -> BTreeSet<String> {
        BTreeSet::from(["hanzo-domains-platform-hanzo-ai".to_string()])
    }

    #[test]
    fn current_generation_shard_is_kept() {
        assert!(!is_superseded(
            "hanzo-domains-platform-hanzo-ai",
            &[CR_UID],
            CR_UID,
            &desired(),
        ));
    }

    #[test]
    fn superseded_index_shard_is_pruned() {
        // `hanzo-domains-4-platform-hanzo-ai` — old index, same CR, not desired.
        assert!(is_superseded(
            "hanzo-domains-4-platform-hanzo-ai",
            &[CR_UID],
            CR_UID,
            &desired(),
        ));
    }

    #[test]
    fn old_scheme_shard_is_pruned() {
        // A second index shard from another reorder — same CR, no longer desired.
        assert!(is_superseded(
            "hanzo-domains-2-platform-hanzo-ai",
            &[CR_UID],
            CR_UID,
            &desired(),
        ));
    }

    #[test]
    fn sibling_cr_shard_is_never_touched() {
        // Owned by a different CR (different uid) — out of scope.
        assert!(!is_superseded(
            "other-domains-0-platform-hanzo-ai",
            &["other-cr-uid"],
            CR_UID,
            &desired(),
        ));
    }

    #[test]
    fn unowned_ingress_is_never_touched() {
        // Carries the managed-by label but no ownerReference to this CR
        // (hand-made or pre-owner-ref) — ownership unproven, so never deleted.
        assert!(!is_superseded(
            "hanzo-domains-9-platform-hanzo-ai",
            &[],
            CR_UID,
            &desired(),
        ));
    }

    #[test]
    fn empty_cr_uid_prunes_nothing() {
        // No CR UID means the sweep cannot scope safely — no-op.
        assert!(!is_superseded(
            "hanzo-domains-4-platform-hanzo-ai",
            &[""],
            "",
            &desired(),
        ));
    }
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
