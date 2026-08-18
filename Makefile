.PHONY: all build release check test lint fmt doc audit clean
.PHONY: coverage-check extended-lint
.PHONY: require-container-engine images container praxis-image
.PHONY: kind-up kind-down kind-reset conformance smoke-test
.PHONY: dev-env dev-conformance dev-cycle dev-integration dev-push
.PHONY: test-integration run
.PHONY: setup-hooks help

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

CONTAINER_ENGINE  ?= $(shell command -v podman 2>/dev/null \
                     || command -v docker 2>/dev/null)
V                 ?=
KIND_CLUSTER_NAME ?= praxis-conformance
PRAXIS_DIR        ?= $(shell cd "$(CURDIR)/../praxis" 2>/dev/null && pwd)
PRAXIS_IMAGE      ?= praxis:dev
OPERATOR_IMAGE    ?= praxis-operator:dev
KUBECTL           ?= kubectl --context kind-$(KIND_CLUSTER_NAME)

ifneq ($(V),)
  _NOCAPTURE := -- --nocapture
endif

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

all: build fmt lint test audit

build:
	cargo build

release:
	cargo build --release

check:
	cargo check

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

lint:
	cargo clippy --all-targets -- -D warnings
	cargo +nightly fmt --all -- --check

extended-lint:
	cargo run -p xtask -- lint-extended

fmt:
	cargo +nightly fmt --all

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items

audit:
	cargo audit
	cargo deny check

clean:
	cargo clean

coverage-check:
	cargo llvm-cov --fail-under-lines 95

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

test:
	cargo test $(_NOCAPTURE)

test-integration:
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

# ---------------------------------------------------------------------------
# Container
# ---------------------------------------------------------------------------

require-container-engine:
ifndef CONTAINER_ENGINE
	$(error No container engine found. Install podman or docker)
endif

container: | require-container-engine
	$(CONTAINER_ENGINE) build -t $(OPERATOR_IMAGE) -f Containerfile .

praxis-image: | require-container-engine
	@if [ ! -d "$(PRAXIS_DIR)" ]; then \
		echo "ERROR: praxis source not found at $(PRAXIS_DIR)"; \
		echo "  Set PRAXIS_DIR to the path of the praxis repository."; \
		exit 1; \
	fi
	$(CONTAINER_ENGINE) build -t $(PRAXIS_IMAGE) \
		-f $(PRAXIS_DIR)/Containerfile $(PRAXIS_DIR)

images: container praxis-image

# ---------------------------------------------------------------------------
# KIND
# ---------------------------------------------------------------------------

kind-up: images
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	PRAXIS_IMAGE=$(PRAXIS_IMAGE) \
	OPERATOR_IMAGE=$(OPERATOR_IMAGE) \
	CONTAINER_ENGINE=$(CONTAINER_ENGINE) \
	bash hack/setup-kind.sh

kind-down:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/teardown-kind.sh

kind-reset:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/reset-cluster.sh

conformance: kind-up
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/run-conformance.sh

smoke-test:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/smoke-test.sh

# ---------------------------------------------------------------------------
# Iterative Development
# ---------------------------------------------------------------------------

dev-push: container
	kind load docker-image $(OPERATOR_IMAGE) --name $(KIND_CLUSTER_NAME)
	sed 's|__PRAXIS_IMAGE__|$(PRAXIS_IMAGE)|g; s|__OPERATOR_IMAGE__|$(OPERATOR_IMAGE)|g' \
		deploy/deployment.yaml | $(KUBECTL) apply -f -
	$(KUBECTL) -n praxis-system rollout restart deployment/praxis-operator
	$(KUBECTL) -n praxis-system rollout status deployment/praxis-operator --timeout=120s

dev-cycle: dev-push kind-reset dev-conformance

dev-env: images
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	PRAXIS_IMAGE=$(PRAXIS_IMAGE) \
	OPERATOR_IMAGE=$(OPERATOR_IMAGE) \
	CONTAINER_ENGINE=$(CONTAINER_ENGINE) \
	bash hack/setup-kind.sh

dev-conformance:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/run-conformance.sh

dev-integration:
	@kind get kubeconfig --name $(KIND_CLUSTER_NAME) \
		> /tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig
	KUBECONFIG=/tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig \
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

run:
	@kind get kubeconfig --name $(KIND_CLUSTER_NAME) \
		> /tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig
	$(KUBECTL) -n praxis-system scale deployment/praxis-operator --replicas=0
	KUBECONFIG=/tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig \
	PRAXIS_IMAGE=$(PRAXIS_IMAGE) \
	RUST_LOG=praxis_operator=debug \
	cargo run

# ---------------------------------------------------------------------------
# Dev Setup
# ---------------------------------------------------------------------------

setup-hooks:
	@ln -sf ../../.hooks/pre-commit .git/hooks/pre-commit
	@echo "Git hooks installed"

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------

help:
	@echo "Variables:"
	@echo "  V=1                show test output (--nocapture)"
	@echo "  CONTAINER_ENGINE   container runtime (auto-detected)"
	@echo "  KIND_CLUSTER_NAME  KIND cluster name (default: praxis-conformance)"
	@echo "  PRAXIS_DIR         path to praxis source (default: ../praxis)"
	@echo "  PRAXIS_IMAGE       praxis container image tag"
	@echo "  OPERATOR_IMAGE     operator container image tag"
	@echo ""
	@echo "Top-level:"
	@echo "  all                build + lint + test + audit"
	@echo ""
	@echo "Build:"
	@echo "  build              cargo build"
	@echo "  release            cargo build --release"
	@echo "  check              cargo check"
	@echo "  clean              cargo clean"
	@echo ""
	@echo "Test:"
	@echo "  test               run all tests"
	@echo "  test-integration   run integration tests (requires integration feature)"
	@echo ""
	@echo "Quality:"
	@echo "  lint               clippy + nightly rustfmt check"
	@echo "  extended-lint      diff-scoped heuristic checks (TODOs, comment slop, repetition)"
	@echo "  fmt                format with nightly rustfmt"
	@echo "  doc                build docs with warnings denied"
	@echo "  audit              cargo audit + cargo deny"
	@echo "  coverage-check     fail if line coverage < 80%%"
	@echo ""
	@echo "Container:"
	@echo "  container          build operator container image"
	@echo "  praxis-image       build praxis container image"
	@echo "  images             build both container images"
	@echo ""
	@echo "KIND:"
	@echo "  kind-up            create cluster + deploy"
	@echo "  kind-down          delete cluster"
	@echo "  kind-reset         reset cluster state"
	@echo "  conformance        run conformance suite (creates cluster)"
	@echo "  smoke-test         run smoke tests"
	@echo ""
	@echo "Development:"
	@echo "  dev-env            create/reuse persistent cluster"
	@echo "  dev-push           build + load + rollout operator"
	@echo "  dev-cycle          dev-push + kind-reset + dev-conformance"
	@echo "  dev-conformance    run conformance (existing cluster)"
	@echo "  dev-integration    run integration tests against cluster"
	@echo "  run                run operator locally against cluster"
	@echo ""
	@echo "Dev Setup:"
	@echo "  setup-hooks        install git pre-commit hook"
