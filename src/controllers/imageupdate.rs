//! ImageUpdate reconciler — registry→git image automation (native GitOps).
//!
//! Retires the `notify-universe` GitHub `repository_dispatch` step (in every
//! service's release.yml) plus universe's `image-update.yml`: instead of CI
//! calling back into git to bump a tag, the operator polls `imageRepository`
//! (registry.hanzo.ai — the canonical fleet registry, NOT ghcr) for tags
//! matching `policy`, and on a newer one writes the bump into `writebackPath`
//! in git. The GitSource controller then applies it — closing
//! build→push→bump→rollout entirely in-cluster.
//!
//! The tag-selection + write-back logic below is the Flux GitOps-toolkit
//! *algorithm* (semver image selection with git write-back), reimplemented
//! natively over reqwest + the shared `gitops` git plumbing — never the fluxcd
//! crates/brand. The selection + rewrite are pure functions, exhaustively
//! table-tested (this is the load-bearing correctness surface: a wrong tag rolls
//! the fleet).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{ImageUpdate, ImageUpdateStatus, Phase};
use crate::crd_types::{build_condition, carry_transition_time, status_changed, upsert_condition};
use crate::gitops::{self, Checkout, ARCH_SUFFIX};
use crate::registry;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure core — tag shape, policy, selection, write-back. No I/O; fully testable.
// ─────────────────────────────────────────────────────────────────────────────

/// The lexical shape of a CR's image tag: an optional `v` prefix and an optional
/// `-<arch>` suffix around a semver core. Write-back preserves the shape and
/// swaps only the core, so cloud's bare `v1.801.16` and cms's `3.86.3-amd64`
/// each keep their own convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagShape {
    pub v_prefix: bool,
    pub arch_suffix: bool,
}

/// Parse a tag into its shape + semver core. `None` when the core is not semver
/// (e.g. a `sha-b2121ab-amd64` content tag — those services cannot be
/// semver-automated and are intentionally not given an ImageUpdate CR).
pub fn parse_tag(tag: &str) -> Option<(TagShape, semver::Version)> {
    let (core0, arch_suffix) = match tag.strip_suffix(ARCH_SUFFIX) {
        Some(c) => (c, true),
        None => (tag, false),
    };
    let (core, v_prefix) = match core0.strip_prefix('v') {
        Some(c) => (c, true),
        None => (core0, false),
    };
    let ver = semver::Version::parse(core).ok()?;
    Some((TagShape { v_prefix, arch_suffix }, ver))
}

/// Render a version back into a concrete tag string in the given shape.
pub fn render_tag(shape: &TagShape, v: &semver::Version) -> String {
    format!(
        "{}{}{}",
        if shape.v_prefix { "v" } else { "" },
        v,
        if shape.arch_suffix { ARCH_SUFFIX } else { "" }
    )
}

/// Tag-selection policy.
pub enum Policy {
    /// Newest semver satisfying a version requirement (`semver:*` → `*` → any).
    Semver(semver::VersionReq),
    /// Newest (by semver core) tag whose raw form matches a regex.
    Regex(regex::Regex),
}

/// Parse the `spec.policy` string. Accepts `semver:*`, `semver:<range>`, a bare
/// range (`>=1.0.0 <2.0.0`, space- or comma-separated), or `regex:<pattern>`.
pub fn parse_policy(s: &str) -> Result<Policy> {
    if let Some(pat) = s.strip_prefix("regex:") {
        let re = regex::Regex::new(pat)
            .map_err(|e| OperatorError::Config(format!("bad regex policy '{s}': {e}")))?;
        return Ok(Policy::Regex(re));
    }
    let range = s.strip_prefix("semver:").unwrap_or(s).trim();
    let normalized = if range.is_empty() || range == "*" {
        "*".to_string()
    } else {
        // semver::VersionReq wants comma-separated comparators; accept spaces.
        range
            .split([',', ' ', '\t'])
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let req = semver::VersionReq::parse(&normalized)
        .map_err(|e| OperatorError::Config(format!("bad semver policy '{s}': {e}")))?;
    Ok(Policy::Semver(req))
}

/// Select the newest deployable tag strictly newer than `current`, rendered in
/// `shape` — or `None` when nothing newer is safely available.
///
/// The arch guard is the load-bearing safety property: a candidate version is
/// deployable only if its concrete pushed image exists for our arch. When the
/// repo uses per-arch tagging (any `-amd64` tag present), we require the
/// `<v>-amd64` tag to exist; that is exactly what stops the dangling-bare-tag
/// bug (`v1.801.11` was listed with no image — its `-amd64` sibling was absent).
/// When the repo publishes only bare (manifest-list) tags, we require the bare
/// tag itself to be listed. Either way we never write a tag with no real image.
pub fn select_new_tag(
    registry_tags: &[String],
    policy: &Policy,
    shape: &TagShape,
    current: &semver::Version,
) -> Option<String> {
    let have: HashSet<&str> = registry_tags.iter().map(String::as_str).collect();
    let arch_mode = registry_tags.iter().any(|t| t.ends_with(ARCH_SUFFIX));

    let mut best: Option<semver::Version> = None;
    for t in registry_tags {
        let Some((_, v)) = parse_tag(t) else { continue };
        let passes = match policy {
            Policy::Semver(req) => req.matches(&v),
            Policy::Regex(re) => re.is_match(t),
        };
        if !passes {
            continue;
        }
        if v <= *current {
            continue; // only ever roll forward — never regress a pinned floor
        }
        // Arch guard, rendered in the CR's own prefix convention.
        let guard = if arch_mode {
            render_tag(&TagShape { v_prefix: shape.v_prefix, arch_suffix: true }, &v)
        } else {
            render_tag(&TagShape { v_prefix: shape.v_prefix, arch_suffix: false }, &v)
        };
        if !have.contains(guard.as_str()) {
            continue;
        }
        if best.as_ref().map_or(true, |b| v > *b) {
            best = Some(v);
        }
    }
    best.map(|v| render_tag(shape, &v))
}

/// The image basename used to locate the right `image:` block in a CR file:
/// the last path segment of the repository (`registry.hanzo.ai/hanzo/cloud` →
/// `cloud`), tag/digest stripped.
pub fn image_basename(image_repository: &str) -> String {
    let last = image_repository
        .rsplit('/')
        .next()
        .unwrap_or(image_repository);
    last.split([':', '@']).next().unwrap_or(last).to_string()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}
/// The indentation of a line whose first key is exactly `key` (`^\s*key:`).
fn key_indent(line: &str, key: &str) -> Option<usize> {
    let t = line.trim_start();
    if t.starts_with(&format!("{key}:")) || t == key {
        Some(indent_of(line))
    } else {
        None
    }
}
/// The value text after `key:` (inline comment + surrounding quotes stripped).
fn key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let t = line.trim_start();
    let rest = t.strip_prefix(key)?.trim_start().strip_prefix(':')?.trim();
    let rest = rest.split(" #").next().unwrap_or(rest).trim();
    Some(rest.trim_matches('"').trim_matches('\''))
}

/// Read the current image tag from a CR file, locating the `image:` block whose
/// `repository` basename matches `image_basename`.
pub fn read_image_tag(content: &str, image_basename: &str) -> Result<String> {
    let (_, value) = locate_tag_line(content, image_basename)?;
    Ok(value)
}

/// Locate the `image.tag` line for the matching image block. Returns
/// (line_index, current_unquoted_value).
fn locate_tag_line(content: &str, image_basename: &str) -> Result<(usize, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some(img_indent) = key_indent(lines[i], "image") else {
            i += 1;
            continue;
        };
        let mut repo_ok = false;
        let mut tag_idx: Option<usize> = None;
        let mut j = i + 1;
        while j < lines.len() {
            let l = lines[j];
            if l.trim().is_empty() || is_comment(l) {
                j += 1;
                continue;
            }
            if indent_of(l) <= img_indent {
                break; // block ended at a sibling/parent key
            }
            if let Some(v) = key_value(l, "repository") {
                let base = v.rsplit('/').next().unwrap_or(v);
                let base = base.split([':', '@']).next().unwrap_or(base);
                if base == image_basename {
                    repo_ok = true;
                }
            }
            if tag_idx.is_none() && key_indent(l, "tag").is_some() {
                tag_idx = Some(j);
            }
            j += 1;
        }
        if repo_ok {
            let idx = tag_idx.ok_or_else(|| {
                OperatorError::Config(format!(
                    "image block for '{image_basename}' has no tag: line"
                ))
            })?;
            let value = key_value(lines[idx], "tag")
                .unwrap_or_default()
                .to_string();
            return Ok((idx, value));
        }
        i = j;
    }
    Err(OperatorError::Config(format!(
        "no image block with repository basename '{image_basename}' in writeback file"
    )))
}

/// Rewrite the `image.tag` value for the matching image block to `new_tag`,
/// preserving every other byte — indentation, quote style, trailing comment, and
/// the surrounding 300 lines of tag-history comments. Returns the new file
/// content, or `None` when the tag already equals `new_tag` (idempotent).
pub fn rewrite_image_tag(
    content: &str,
    image_basename: &str,
    new_tag: &str,
) -> Result<Option<String>> {
    let (idx, current) = locate_tag_line(content, image_basename)?;
    if current == new_tag {
        return Ok(None);
    }
    let lines: Vec<&str> = content.lines().collect();
    let new_line = swap_tag_value(lines[idx], new_tag);
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    out[idx] = new_line;
    let mut joined = out.join("\n");
    if content.ends_with('\n') {
        joined.push('\n');
    }
    Ok(Some(joined))
}

/// Replace only the value token of a `  tag: <value>` line, keeping the exact
/// prefix (`  tag: `), the old quote style, and any trailing ` # comment`.
fn swap_tag_value(line: &str, new_tag: &str) -> String {
    // Split into the "  tag:" key part and the remainder after it.
    let colon = line.find("tag:").map(|p| p + 4).unwrap_or(0);
    let (prefix_key, remainder) = line.split_at(colon);
    // Leading spaces of the value region (kept verbatim).
    let ws_len = remainder.len() - remainder.trim_start().len();
    let (lead_ws, value_region) = remainder.split_at(ws_len);

    // Determine quote style + where the value token ends.
    let (quote, token_body): (&str, &str) = if let Some(r) = value_region.strip_prefix('"') {
        ("\"", r.split('"').next().unwrap_or(r))
    } else if let Some(r) = value_region.strip_prefix('\'') {
        ("'", r.split('\'').next().unwrap_or(r))
    } else {
        ("", value_region.split([' ', '\t', '#']).next().unwrap_or(value_region))
    };
    let token_len = quote.len() * 2 + token_body.len();
    let trailing = &value_region[token_len.min(value_region.len())..];

    format!("{prefix_key}{lead_ws}{quote}{new_tag}{quote}{trailing}")
}

// ─────────────────────────────────────────────────────────────────────────────
// Reconcile
// ─────────────────────────────────────────────────────────────────────────────

pub async fn reconcile(cr: Arc<ImageUpdate>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("ImageUpdate has no namespace".into()))?;
    let spec = &cr.spec;
    let generation = cr.meta().generation.unwrap_or(0);

    if spec.image_repository.is_empty() || spec.writeback_path.is_empty() {
        warn!(name = %name, namespace = %namespace, "ImageUpdate missing imageRepository or writebackPath");
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    let interval = Duration::from_secs(spec.interval_seconds.max(60));
    let prior = cr.status.clone().unwrap_or_default();

    match run_update(&ctx.client, &namespace, spec).await {
        Ok(outcome) => {
            let mut status = ImageUpdateStatus {
                phase: Some(Phase::Running),
                latest_tag: outcome.latest_tag.clone(),
                last_pushed_tag: outcome
                    .pushed_tag
                    .clone()
                    .unwrap_or_else(|| prior.last_pushed_tag.clone()),
                last_push_time: outcome
                    .pushed_tag
                    .as_ref()
                    .map(|_| jiff::Timestamp::now().to_string())
                    .or(prior.last_push_time.clone()),
                conditions: prior.conditions.clone(),
                observed_generation: generation,
                message: outcome.message.clone(),
            };
            let mut cond = build_condition("Synced", true, "SyncOk", &outcome.message, generation);
            carry_transition_time(&prior.conditions, &mut cond);
            upsert_condition(&mut status.conditions, cond);
            if let Some(t) = &outcome.pushed_tag {
                info!(name = %name, namespace = %namespace, image = %spec.image_repository, tag = %t, "ImageUpdate pushed tag bump");
            }
            write_status(&ctx.client, &namespace, &name, &status, &prior).await;
            Ok(Action::requeue(interval))
        }
        Err(e) => {
            let msg = e.to_string();
            let mut status = ImageUpdateStatus {
                phase: Some(Phase::Degraded),
                latest_tag: prior.latest_tag.clone(),
                last_pushed_tag: prior.last_pushed_tag.clone(),
                last_push_time: prior.last_push_time.clone(),
                conditions: prior.conditions.clone(),
                observed_generation: generation,
                message: msg.clone(),
            };
            let mut cond = build_condition("Synced", false, "SyncFailed", &msg, generation);
            carry_transition_time(&prior.conditions, &mut cond);
            upsert_condition(&mut status.conditions, cond);
            write_status(&ctx.client, &namespace, &name, &status, &prior).await;
            Err(e)
        }
    }
}

/// Result of one poll: what the newest matching tag is, whether we pushed, and a
/// human message.
struct Outcome {
    latest_tag: String,
    pushed_tag: Option<String>,
    message: String,
}

async fn run_update(
    client: &Client,
    namespace: &str,
    spec: &crate::crd::ImageUpdateSpec,
) -> Result<Outcome> {
    let policy = parse_policy(&spec.policy)?;
    let token = if spec.credentials_secret.is_empty() {
        String::new()
    } else {
        gitops::read_secret_key(client, namespace, &spec.credentials_secret, "token").await?
    };
    let dockercfg = if spec.registry_secret.is_empty() {
        None
    } else {
        Some(
            gitops::read_secret_key(
                client,
                namespace,
                &spec.registry_secret,
                ".dockerconfigjson",
            )
            .await?,
        )
    };

    // Clone the writeback repo shallow and read the current tag from the file —
    // the file is the source of truth, so a poll is idempotent against manual
    // edits, not just against our own last push.
    let checkout = Checkout::clone(&spec.writeback_repo, &spec.writeback_ref, &token).await?;
    let file_path = checkout.join(&spec.writeback_path);
    let content = tokio::fs::read_to_string(&file_path).await.map_err(|e| {
        OperatorError::Reconcile(format!("read {}: {e}", spec.writeback_path))
    })?;

    let basename = image_basename(&spec.image_repository);
    let current_tag = read_image_tag(&content, &basename)?;
    let Some((shape, current_ver)) = parse_tag(&current_tag) else {
        // A non-semver tag (e.g. sha-based) cannot be semver-selected. Report it
        // and stop — this is a config mismatch, not a transient failure.
        return Err(OperatorError::Config(format!(
            "current tag '{current_tag}' for '{basename}' is not semver — ImageUpdate needs a semver tag scheme"
        )));
    };

    let tags = registry::list_tags(&spec.image_repository, dockercfg.as_deref()).await?;
    let selected = select_new_tag(&tags, &policy, &shape, &current_ver);

    let latest_tag = selected.clone().unwrap_or_else(|| current_tag.clone());
    let Some(new_tag) = selected else {
        return Ok(Outcome {
            latest_tag,
            pushed_tag: None,
            message: format!("at latest: {current_tag}"),
        });
    };

    // Rewrite → write → commit → push. rewrite_image_tag double-guards idempotency.
    let Some(new_content) = rewrite_image_tag(&content, &basename, &new_tag)? else {
        return Ok(Outcome {
            latest_tag,
            pushed_tag: None,
            message: format!("at latest: {current_tag}"),
        });
    };
    tokio::fs::write(&file_path, new_content)
        .await
        .map_err(|e| OperatorError::Reconcile(format!("write {}: {e}", spec.writeback_path)))?;
    let message = format!("gitops: bump {} -> {}", spec.image_repository, new_tag);
    checkout
        .commit_and_push(&[&spec.writeback_path], &message, &spec.writeback_ref)
        .await?;

    Ok(Outcome {
        latest_tag: new_tag.clone(),
        pushed_tag: Some(new_tag),
        message,
    })
}

async fn write_status(
    client: &Client,
    namespace: &str,
    name: &str,
    status: &ImageUpdateStatus,
    prior: &ImageUpdateStatus,
) {
    if !status_changed(status, prior) {
        return;
    }
    let api: Api<ImageUpdate> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update ImageUpdate status (CRD may not be installed)");
    }
}

pub fn on_error(_obj: Arc<ImageUpdate>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "ImageUpdate reconcile failed");
    Action::requeue(Duration::from_secs(60))
}

pub async fn run_imageupdate_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<ImageUpdate> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting ImageUpdate controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn parse_tag_shapes() {
        let (s, v) = parse_tag("v1.801.16").unwrap();
        assert!(s.v_prefix && !s.arch_suffix && v == ver("1.801.16"));
        let (s, v) = parse_tag("3.86.3-amd64").unwrap();
        assert!(!s.v_prefix && s.arch_suffix && v == ver("3.86.3"));
        let (s, v) = parse_tag("v2.0.0-amd64").unwrap();
        assert!(s.v_prefix && s.arch_suffix && v == ver("2.0.0"));
        // Non-semver content tags are rejected (studio/world sha tags).
        assert!(parse_tag("sha-b2121ab-amd64").is_none());
        assert!(parse_tag("latest").is_none());
    }

    #[test]
    fn render_tag_roundtrips_shape() {
        let (s, v) = parse_tag("v1.801.16").unwrap();
        assert_eq!(render_tag(&s, &v), "v1.801.16");
        let (s, v) = parse_tag("3.86.3-amd64").unwrap();
        assert_eq!(render_tag(&s, &v), "3.86.3-amd64");
    }

    #[test]
    fn policy_parsing() {
        assert!(matches!(parse_policy("semver:*").unwrap(), Policy::Semver(_)));
        assert!(matches!(parse_policy("regex:^v.*").unwrap(), Policy::Regex(_)));
        // Space-separated range normalizes to comma-separated for semver crate.
        assert!(matches!(
            parse_policy(">=1.0.0 <2.0.0").unwrap(),
            Policy::Semver(_)
        ));
        assert!(parse_policy("regex:(").is_err());
        assert!(parse_policy("semver:not-a-range").is_err());
    }

    // ---- selection: newest semver ----
    #[test]
    fn selects_newest_semver_bare_repo() {
        // No -amd64 tags anywhere → bare-tag mode; guard requires the bare tag.
        let tags = vec!["v1.0.0".into(), "v1.2.0".into(), "v1.1.0".into()];
        let (shape, cur) = parse_tag("v1.0.0").unwrap();
        let policy = parse_policy("semver:*").unwrap();
        assert_eq!(select_new_tag(&tags, &policy, &shape, &cur), Some("v1.2.0".into()));
    }

    // ---- selection: range bound ----
    #[test]
    fn respects_semver_range_upper_bound() {
        let tags = vec!["1.0.0".into(), "1.9.0".into(), "2.0.0".into()];
        let (shape, cur) = parse_tag("1.0.0").unwrap();
        let policy = parse_policy(">=1.0.0 <2.0.0").unwrap();
        assert_eq!(select_new_tag(&tags, &policy, &shape, &cur), Some("1.9.0".into()));
    }

    // ---- selection: regex ----
    #[test]
    fn regex_policy_filters_then_orders_by_semver() {
        // Regex NARROWS the candidate set (semver core still orders + guards).
        // Here it admits clean releases and excludes prereleases.
        let tags = vec![
            "v1.0.0".into(),
            "v1.5.0".into(),
            "v1.5.0-rc1".into(), // prerelease — excluded by the pattern
        ];
        let policy = parse_policy(r"regex:^v\d+\.\d+\.\d+$").unwrap();
        let (shape, cur) = parse_tag("v1.0.0").unwrap();
        assert_eq!(
            select_new_tag(&tags, &policy, &shape, &cur),
            Some("v1.5.0".into())
        );
    }

    // ---- selection: arch-suffix guard (the v1.801.11 bug) ----
    #[test]
    fn arch_guard_skips_dangling_bare_tag() {
        // Repo uses per-arch tagging. v1.801.11 is listed BARE with no -amd64
        // sibling (the dangling/partial-push bug). v1.801.10 has its -amd64.
        let tags = vec![
            "v1.801.10".into(),
            "v1.801.10-amd64".into(),
            "v1.801.11".into(), // dangling: no -amd64 → must be skipped
        ];
        let (shape, cur) = parse_tag("v1.801.9").unwrap();
        let policy = parse_policy("semver:*").unwrap();
        assert_eq!(
            select_new_tag(&tags, &policy, &shape, &cur),
            Some("v1.801.10".into())
        );
    }

    #[test]
    fn arch_guard_writes_amd64_shape_for_suffixed_cr() {
        // cms consumes `<semver>-amd64`; selection must produce the -amd64 tag and
        // only when it exists.
        let tags = vec![
            "3.86.3".into(),
            "3.86.3-amd64".into(),
            "3.87.0".into(),
            "3.87.0-amd64".into(),
        ];
        let (shape, cur) = parse_tag("3.86.3-amd64").unwrap();
        let policy = parse_policy("semver:*").unwrap();
        assert_eq!(
            select_new_tag(&tags, &policy, &shape, &cur),
            Some("3.87.0-amd64".into())
        );
    }

    #[test]
    fn no_image_for_tag_guard_blocks_when_only_bare_partial() {
        // Only a bare newer tag exists but repo IS in arch mode (an -amd64 exists
        // for the OLD version). The newer bare tag has no -amd64 → no update.
        let tags = vec![
            "1.0.0".into(),
            "1.0.0-amd64".into(),
            "1.1.0".into(), // newer but no -amd64 → skipped
        ];
        let (shape, cur) = parse_tag("1.0.0-amd64").unwrap();
        let policy = parse_policy("semver:*").unwrap();
        assert_eq!(select_new_tag(&tags, &policy, &shape, &cur), None);
    }

    #[test]
    fn never_regresses_below_current() {
        let tags = vec!["1.0.0".into(), "0.9.0".into(), "2.0.0".into()];
        let (shape, cur) = parse_tag("2.0.0").unwrap();
        let policy = parse_policy("semver:*").unwrap();
        assert_eq!(select_new_tag(&tags, &policy, &shape, &cur), None);
    }

    // ---- write-back: precise textual edit ----
    #[test]
    fn rewrite_preserves_quotes_and_comment_history() {
        // Mirrors cloud.yaml: repository, a big comment gap, then a quoted tag.
        let f = "\
apiVersion: hanzo.ai/v1
kind: App
metadata:
  name: cloud
spec:
  image:
    repository: ghcr.io/hanzoai/cloud
    # 1.785.29 = trust boundary
    # v1.801.16 = mission-control
    tag: \"v1.801.16\"
    pullPolicy: IfNotPresent
";
        let out = rewrite_image_tag(f, "cloud", "v1.802.0").unwrap().unwrap();
        assert!(out.contains("    tag: \"v1.802.0\"\n"));
        // Comment history untouched.
        assert!(out.contains("# v1.801.16 = mission-control"));
        assert!(out.contains("    repository: ghcr.io/hanzoai/cloud"));
        assert!(out.ends_with("pullPolicy: IfNotPresent\n"));
    }

    #[test]
    fn rewrite_unquoted_suffixed_tag() {
        let f = "\
spec:
  image:
    repository: ghcr.io/hanzoai/cms
    tag: 3.86.3-amd64
";
        let out = rewrite_image_tag(f, "cms", "3.87.0-amd64").unwrap().unwrap();
        assert!(out.contains("    tag: 3.87.0-amd64\n"));
    }

    #[test]
    fn rewrite_preserves_trailing_comment() {
        let f = "  image:\n    repository: x/cloud\n    tag: \"v1.0.0\"  # pinned\n";
        let out = rewrite_image_tag(f, "cloud", "v1.1.0").unwrap().unwrap();
        assert_eq!(out, "  image:\n    repository: x/cloud\n    tag: \"v1.1.0\"  # pinned\n");
    }

    #[test]
    fn rewrite_is_idempotent_when_equal() {
        let f = "  image:\n    repository: x/cloud\n    tag: v1.0.0\n";
        assert_eq!(rewrite_image_tag(f, "cloud", "v1.0.0").unwrap(), None);
    }

    #[test]
    fn rewrite_targets_correct_block_among_many() {
        // A sidecar image block must not be touched; match by repository basename.
        let f = "\
spec:
  image:
    repository: ghcr.io/hanzoai/cloud
    tag: v1.0.0
  sidecars:
  - image:
      repository: ghcr.io/hanzoai/otel
      tag: v9.9.9
";
        let out = rewrite_image_tag(f, "cloud", "v1.1.0").unwrap().unwrap();
        assert!(out.contains("repository: ghcr.io/hanzoai/cloud\n    tag: v1.1.0"));
        assert!(out.contains("tag: v9.9.9")); // sidecar untouched
    }

    #[test]
    fn rewrite_errors_on_missing_block() {
        let f = "spec:\n  image:\n    repository: x/other\n    tag: v1.0.0\n";
        assert!(rewrite_image_tag(f, "cloud", "v2.0.0").is_err());
    }

    #[test]
    fn read_image_tag_locates_value() {
        let f = "spec:\n  image:\n    repository: r/cloud\n    tag: \"v1.801.16\"\n";
        assert_eq!(read_image_tag(f, "cloud").unwrap(), "v1.801.16");
    }

    #[test]
    fn image_basename_strips_registry_and_tag() {
        assert_eq!(image_basename("registry.hanzo.ai/hanzo/cloud"), "cloud");
        assert_eq!(image_basename("ghcr.io/hanzoai/cms:3.0.0"), "cms");
        assert_eq!(image_basename("cloud"), "cloud");
    }
}
