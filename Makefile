IMG ?= ghcr.io/hanzoai/hanzo-operator:latest
BINARY ?= hanzo-operator

.PHONY: build test lint fmt vet docker-build docker-push deploy undeploy install uninstall run

##@ Build

build: fmt vet ## Build the operator binary.
	go build -o bin/$(BINARY) .

run: fmt vet ## Run against the configured cluster.
	go run . --health-probe-bind-address=:8082 --metrics-bind-address=:8081

fmt: ## Format code.
	go fmt ./...

vet: ## Vet code.
	go vet ./...

lint: ## Lint code.
	golangci-lint run ./...

test: ## Run unit tests.
	go test -v -race ./...

##@ Docker

docker-build: ## Build docker image.
	docker build -t $(IMG) .

docker-push: ## Push docker image.
	docker push $(IMG)

##@ Deploy

install: ## Install CRDs into the cluster.
	kubectl apply -f config/crd/

uninstall: ## Uninstall CRDs from the cluster.
	kubectl delete -f config/crd/

deploy: install ## Deploy operator to the cluster.
	kubectl apply -f config/rbac/
	kubectl apply -f config/manager/

undeploy: ## Remove operator from the cluster.
	kubectl delete -f config/manager/
	kubectl delete -f config/rbac/
	kubectl delete -f config/crd/

sample: ## Apply sample GatewayConfig.
	kubectl apply -f config/samples/

##@ Help

help: ## Show this help.
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)
