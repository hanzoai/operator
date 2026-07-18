//! Shared git plumbing for the ImageUpdate controller (registry→git write-back).
//! ImageUpdate write-back).
//!
//! Shells the `git` binary — the SAME mechanism the retired `gitops-reconcile`
//! cron used (`infra/k8s/gitops-reconcile/reconcile.sh`) — so there is ONE git
//! strategy across clone + commit + push, with no libgit2/gitoxide dependency
//! to vendor. git is well-tested and already the established tool for this exact
//! job; reimplementing its transport in-process would be the framework-worship
//! anti-pattern.
//!
//! ## Token safety (stricter than the cron)
//! The PAT is injected via `GIT_ASKPASS` — passed to the child as an env var,
//! never on `argv` (world-visible in `ps`/`/proc`) and never persisted to
//! `.git/config`. The remote URL carries only the *username* (`x-access-token`),
//! not the secret. Every error string is scrubbed of the token before it can
//! reach a log line or a CR status.
//!
//! ## Workdir
//! Clones land in a unique dir under `$GITOPS_WORKDIR` (an emptyDir the operator
//! mounts, because its root filesystem is read-only) — falling back to the OS
//! temp dir — and are removed on `Drop`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use kube::Client;
use tokio::process::Command;

use crate::core::{OperatorError, Result};

/// Read one key from a namespaced Secret as UTF-8. The credential secrets both
/// GitOps controllers consume — the git PAT (`token`) and the registry
/// dockerconfig — are KMS-synced Opaque Secrets; k8s-openapi hands back the
/// already-base64-decoded bytes in `.data`.
pub async fn read_secret_key(
    client: &Client,
    namespace: &str,
    name: &str,
    key: &str,
) -> Result<String> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = api
        .get(name)
        .await
        .map_err(|e| OperatorError::Reconcile(format!("get secret {namespace}/{name}: {e}")))?;
    let bytes = secret
        .data
        .and_then(|mut d| d.remove(key))
        .ok_or_else(|| {
            OperatorError::Reconcile(format!("secret {namespace}/{name} has no key '{key}'"))
        })?;
    String::from_utf8(bytes.0).map_err(|e| {
        OperatorError::Reconcile(format!("secret {namespace}/{name} key '{key}' not utf-8: {e}"))
    })
}

/// Deploy-arch tag suffix. The fleet is linux/amd64 (the operator image and the
/// DOKS nodes), and per-arch build tags carry this suffix — its presence is the
/// proof an image actually pushed for our arch. Hard-coded because it does not
/// vary between environments (philosophy: configure only what must vary).
pub const ARCH_SUFFIX: &str = "-amd64";

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Base directory for clones. `$GITOPS_WORKDIR` points at a writable emptyDir in
/// the operator pod (root fs is read-only); tests + local runs fall back to the
/// OS temp dir.
fn workdir_base() -> PathBuf {
    std::env::var_os("GITOPS_WORKDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Replace every occurrence of `token` with `***`. Applied to all command
/// output before it is surfaced. A no-op for the empty (public-repo) token.
pub fn scrub(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "***")
}

/// Strip a scheme + trailing slash so `repo` inputs like
/// `https://github.com/hanzoai/universe`, `github.com/hanzoai/universe`, and a
/// trailing-slash variant all normalize to `github.com/hanzoai/universe`.
fn host_path(repo: &str) -> String {
    repo.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// A shallow working clone in a unique temp dir, removed on `Drop`.
pub struct Checkout {
    /// Unique parent dir (holds the askpass helper + the clone); removed on drop.
    base: PathBuf,
    /// The working tree (`base/repo`).
    repo: PathBuf,
    /// askpass helper path when a token is in play (None for public repos).
    askpass: Option<PathBuf>,
    /// The PAT — kept for the push env and for output scrubbing.
    token: String,
}

impl Checkout {
    /// Clone `repo@git_ref` shallow (depth 1, single branch). `token` may be
    /// empty for a public repo.
    pub async fn clone(repo: &str, git_ref: &str, token: &str) -> Result<Checkout> {
        let hp = host_path(repo);
        let uniq = format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let base = workdir_base().join(format!("gitops-{uniq}"));
        // A stale dir from a crashed prior run must not fail the clone.
        let _ = tokio::fs::remove_dir_all(&base).await;
        tokio::fs::create_dir_all(&base)
            .await
            .map_err(|e| OperatorError::Reconcile(format!("create workdir: {e}")))?;
        let repo_dir = base.join("repo");

        // Username (never the token) in the URL; askpass supplies the password.
        let url = if token.is_empty() {
            format!("https://{hp}")
        } else {
            format!("https://x-access-token@{hp}")
        };

        let askpass = if token.is_empty() {
            None
        } else {
            let p = base.join("askpass.sh");
            tokio::fs::write(&p, b"#!/bin/sh\nexec printf '%s\\n' \"$GITOPS_TOKEN\"\n")
                .await
                .map_err(|e| OperatorError::Reconcile(format!("write askpass: {e}")))?;
            set_exec(&p)?;
            Some(p)
        };

        let co = Checkout {
            base,
            repo: repo_dir.clone(),
            askpass,
            token: token.to_string(),
        };

        let mut cmd = co.git();
        cmd.arg("clone")
            .arg("--depth")
            .arg("1")
            .arg("--single-branch")
            .arg("--branch")
            .arg(git_ref)
            .arg(&url)
            .arg(&repo_dir);
        run(cmd, token, "git clone").await?;
        Ok(co)
    }

    /// The working-tree root.
    pub fn path(&self) -> &Path {
        &self.repo
    }

    /// Absolute path of a repo-relative path.
    pub fn join(&self, rel: &str) -> PathBuf {
        self.repo.join(rel)
    }

    /// Short SHA of the cloned HEAD.
    pub async fn head_short_sha(&self) -> Result<String> {
        let mut cmd = self.git();
        cmd.current_dir(&self.repo)
            .arg("rev-parse")
            .arg("--short")
            .arg("HEAD");
        Ok(run_capture(cmd, &self.token, "git rev-parse").await?.trim().to_string())
    }

    /// Stage `rel_paths`, commit as the operator identity, and push to
    /// `origin HEAD:<git_ref>`. Identity flags are argv-safe (no secret).
    pub async fn commit_and_push(&self, rel_paths: &[&str], message: &str, git_ref: &str) -> Result<()> {
        let mut add = self.git();
        add.current_dir(&self.repo).arg("add");
        for p in rel_paths {
            add.arg(p);
        }
        run(add, &self.token, "git add").await?;

        let mut commit = self.git();
        commit
            .current_dir(&self.repo)
            .arg("-c")
            .arg("user.name=hanzo-operator")
            .arg("-c")
            .arg("user.email=operator@hanzo.ai")
            .arg("commit")
            .arg("-m")
            .arg(message);
        run(commit, &self.token, "git commit").await?;

        let mut push = self.git();
        push.current_dir(&self.repo)
            .arg("push")
            .arg("origin")
            .arg(format!("HEAD:{git_ref}"));
        run(push, &self.token, "git push").await?;
        Ok(())
    }

    /// A `git` command pre-wired with the isolated environment: no interactive
    /// prompt, askpass credential, token via env (never argv), and no ambient
    /// user/system git config bleeding in.
    fn git(&self) -> Command {
        let mut c = Command::new("git");
        c.env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", &self.base)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .kill_on_drop(true);
        if let Some(ap) = &self.askpass {
            c.env("GIT_ASKPASS", ap).env("GITOPS_TOKEN", &self.token);
        }
        c
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup — Drop can't await.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Mark a helper script executable (0700).
#[cfg(unix)]
fn set_exec(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| OperatorError::Reconcile(format!("chmod askpass: {e}")))
}
#[cfg(not(unix))]
fn set_exec(_p: &Path) -> Result<()> {
    Ok(())
}

/// Run a git command, discarding stdout, mapping a non-zero exit to a scrubbed
/// error. `what` names the step for the error message.
async fn run(mut cmd: Command, token: &str, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .await
        .map_err(|e| OperatorError::Reconcile(format!("{what}: spawn failed: {e} (is `git` on PATH in the operator image?)")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(OperatorError::Reconcile(format!(
            "{what} failed: {}",
            scrub(stderr.trim(), token)
        )));
    }
    Ok(())
}

/// Run a git command and return its scrubbed stdout.
async fn run_capture(mut cmd: Command, token: &str, what: &str) -> Result<String> {
    let out = cmd
        .output()
        .await
        .map_err(|e| OperatorError::Reconcile(format!("{what}: spawn failed: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(OperatorError::Reconcile(format!(
            "{what} failed: {}",
            scrub(stderr.trim(), token)
        )));
    }
    Ok(scrub(&String::from_utf8_lossy(&out.stdout), token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_path_normalizes_schemes_and_slash() {
        assert_eq!(host_path("https://github.com/hanzoai/universe"), "github.com/hanzoai/universe");
        assert_eq!(host_path("github.com/hanzoai/universe"), "github.com/hanzoai/universe");
        assert_eq!(host_path("http://github.com/hanzoai/universe/"), "github.com/hanzoai/universe");
        assert_eq!(host_path("  github.com/hanzoai/universe  "), "github.com/hanzoai/universe");
    }

    #[test]
    fn scrub_hides_token_everywhere() {
        let t = "ghp_SECRET123";
        let msg = format!("fatal: could not read https://x-access-token:{t}@github.com/x");
        let s = scrub(&msg, t);
        assert!(!s.contains(t));
        assert!(s.contains("***"));
    }

    #[test]
    fn scrub_empty_token_is_passthrough() {
        assert_eq!(scrub("nothing to hide", ""), "nothing to hide");
    }
}
