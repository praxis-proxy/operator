.PHONY: all build release test lint fmt doc audit clean
.PHONY: images container praxis-image
.PHONY: kind-up kind-down kind-reset conformance smoke-test
.PHONY: dev-env dev-conformance dev-cycle dev-integration dev-push
.PHONY: test-integration run

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

CONTAINER_ENGINE  ?= $(shell command -v podman 2>/dev/null \
                     || command -v docker 2>/dev/null)
KIND_CLUSTER_NAME ?= praxis-conformance
PRAXIS_DIR        ?= $(shell cd "$(CURDIR)/../praxis" 2>/dev/null && pwd)
PRAXIS_IMAGE      ?= praxis:dev
OPERATOR_IMAGE    ?= praxis-operator:dev
KUBECTL           ?= kubectl --context kind-$(KIND_CLUSTER_NAME)

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

all: build fmt lint test audit

build:
	cargo build

release:
	cargo build --release

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

lint:
	cargo clippy --all-targets -- -D warnings
	cargo +nightly fmt --all -- --check

fmt:
	cargo +nightly fmt --all

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items

audit:
	cargo audit
	cargo deny check

clean:
	cargo clean

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

test:
	cargo test

test-integration:
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

# ---------------------------------------------------------------------------
# Container
# ---------------------------------------------------------------------------

container:
	$(CONTAINER_ENGINE) build -t $(OPERATOR_IMAGE) -f Containerfile .

praxis-image:
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
