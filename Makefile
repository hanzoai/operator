# hanzoai/operator — the canonical Rust operator. Two binaries: operator (the
# controller) and generate-crd-yaml. Deploy is not here and never will be:
# universe records the desired tag and CD reconciles it (see hanzo.yml).

CARGO ?= cargo

.PHONY: help build test lint clean

help: ## Show this help.
	@awk 'BEGIN{FS=":.*##";printf "\nUsage: make <target>\n\nTargets:\n"} /^[a-zA-Z_-]+:.*##/{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# --release because that is what SHIPS: the Dockerfile builds
# target/release/{operator,generate-crd-yaml} and copies exactly those two paths
# into the runtime image. A debug build here would not be the same binary.
build: ## Build both binaries the image ships, into target/release.
	$(CARGO) build --release

# The exact command hanzo.yml declares for CI, so the two cannot drift apart.
# The whole lib, no filter: `cargo test --lib <name>` with a stale name prints
# "running 0 tests" and exits 0, which is a gate that has quietly stopped.
test: ## Run the unit tests — the same command CI runs.
	$(CARGO) test --lib

lint: ## clippy across the workspace.
	$(CARGO) clippy --all-targets

# cargo clean removes ./target and nothing else, which is exactly the generated
# output — .gitignore lists /target, root-anchored, and nothing tracked lives there.
clean: ## Remove target/.
	$(CARGO) clean
