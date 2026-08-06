# Canonical Rust implementation of the Hanzo operator.
#
# Build from the repo root: `docker build .`
# Image: ghcr.io/hanzoai/operator:vX.Y.Z (Go sibling: ghcr.io/luxfi/operator).
#
# Builder — Rust 1.95: deps (clap_lex 1.1.0 via clap 4.6) require edition2024
# (stabilized in 1.85); pinned to the toolchain that builds the workspace.
FROM rust:1.95-bookworm AS builder

# Bound rustc parallelism, because nothing else here does.
#
# cargo sizes -j from the CPUs it can SEE. Inside a buildx step that is the
# NODE's core count (8), not the runner pod's 6-CPU / 26Gi cgroup — a build
# container gets a clean environment, so the `CARGO_BUILD_JOBS: "4"` the
# git-runner fleet sets for its own test gates (universe:
# infra/k8s/git-runner/config.yaml) never reaches this compile. Eight
# concurrent rustc, each peaking in LLVM codegen, against a 26Gi ceiling that
# up to ten runners on one 62.8Gi node are already sharing.
#
# The failure does NOT look like memory. The kubelet sees nothing, because the
# process that dies is inside the pod's own dockerd: the log just stops, and
# the only tell is `(signal: 9, SIGKILL: kill)` followed by
# `error: could not compile <crate>` — where the crate named is whichever one
# happened to be resident, not one with anything wrong with it. Measured here
# on run 36785, which died on `jiff` after 181s.
ENV CARGO_BUILD_JOBS=4

WORKDIR /build

# Copy Cargo manifest and lock first for layer caching.
COPY Cargo.toml Cargo.lock ./

# Pre-build a dummy binary so dependencies cache between builds.
RUN mkdir -p src src/bin && \
    echo 'fn main(){}' > src/main.rs && \
    echo 'fn main(){}' > src/bin/generate_crd_yaml.rs && \
    echo '' > src/lib.rs && \
    cargo build --release 2>/dev/null || true

# Now copy real source and build.
COPY src/ src/
RUN touch src/main.rs src/lib.rs src/bin/generate_crd_yaml.rs && cargo build --release

# Runtime
FROM debian:bookworm-slim

# `git` is required at runtime by the native-GitOps controllers (GitSource
# pull-sync + ImageUpdate write-back shell it via src/gitops.rs — the same
# mechanism the retired reconcile cron used). ca-certificates + libssl3 cover
# TLS for the kube client and git-over-HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 git && \
    rm -rf /var/lib/apt/lists/*

RUN useradd -r -u 65532 -g nogroup -s /sbin/nologin operator

COPY --from=builder /build/target/release/operator /usr/local/bin/operator
COPY --from=builder /build/target/release/generate-crd-yaml /usr/local/bin/generate-crd-yaml

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/operator"]
