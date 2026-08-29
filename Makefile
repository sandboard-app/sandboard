# sandboard — API (Rust) + UI (Vite/React)
#
#   make          build both (cargo binary + web/dist for :8080)
#   make run      build both, then serve on :8080
#   make dev      watchexec → cargo run (API hot-reload on :8080)
#   make dev-ui   Vite hot-reload on :5173 (proxies API to :8080)
#   make test     cargo + web unit tests

.PHONY: all build api release ui install-ui run dev dev-ui docs docs-mermaid-assets docs-serve test test-api test-ui clippy sandbox sandbox-push image image-push clean help

all: build

help:
	@echo "Targets:"
	@echo "  make / make build   Build API (debug) and UI into web/dist"
	@echo "  make api            cargo build (debug — fast iterate)"
	@echo "  make release        cargo build --release"
	@echo "  make ui             npm build → web/dist (served by the API)"
	@echo "  make run            Build both, then cargo run (debug)"
	@echo "  make dev            watchexec → cargo run (API hot-reload on :8080)"
	@echo "  make dev-ui         Vite dev server (:5173 → :8080)"
	@echo "  make docs           mdbook build → target/mdbook (fetches mermaid JS)"
	@echo "  make docs-serve     mdbook serve (http://localhost:3000)"
	@echo "  make sandbox        Rebuild all sandbox-<engine> images via podman (CONTAINER_ENGINE=docker to override)"
	@echo "  make sandbox-push   Build and push all sandbox-<engine> images to REGISTRY"
	@echo "  make image          Build the sandboard board image ($(REGISTRY)/sandboard:latest)"
	@echo "  make image-push     Build and push the sandboard board image"
	@echo "  make test           cargo nextest/test + web tests"
	@echo "  make clippy         cargo clippy -D warnings"
	@echo "  make clean          cargo clean + remove web/dist"

build: api ui

api:
	cargo build

release:
	cargo build --release

install-ui:
	npm --prefix web install

ui: install-ui
	npm --prefix web run build

run: build
	cargo run

# Rebuild + restart the API when Rust/config/migration sources change.
# Pair with `make dev-ui` for Vite on :5173. Requires `brew install watchexec`
# (or `cargo install watchexec-cli`).
dev:
	@command -v watchexec >/dev/null 2>&1 || { \
		echo "watchexec not found. Install: brew install watchexec   # or: cargo install watchexec-cli"; \
		exit 1; \
	}
	watchexec \
		-r \
		-w src \
		-w Cargo.toml \
		-w Cargo.lock \
		-w migrations \
		-w sandbox \
		-i target \
		-i web \
		-- cargo run

dev-ui: install-ui
	npm --prefix web run dev

test: test-api test-ui

test-api:
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --offline; \
	else \
		cargo test --offline; \
	fi

test-ui: install-ui
	npm --prefix web test

clippy:
	cargo clippy --all-targets --offline -- -D warnings

# Mermaid browser assets are not committed — mdbook-mermaid install drops
# mermaid.min.js / mermaid-init.js next to book.toml (gitignored).
docs-mermaid-assets:
	@command -v mdbook-mermaid >/dev/null 2>&1 || { \
		echo "mdbook-mermaid not found. Install: cargo install mdbook-mermaid"; \
		exit 1; \
	}
	mdbook-mermaid install .

docs:
	@command -v mdbook >/dev/null 2>&1 || { \
		echo "mdbook not found. Install: brew install mdbook   # or: cargo install mdbook"; \
		exit 1; \
	}
	$(MAKE) docs-mermaid-assets
	mdbook build

docs-serve:
	@command -v mdbook >/dev/null 2>&1 || { \
		echo "mdbook not found. Install: brew install mdbook   # or: cargo install mdbook"; \
		exit 1; \
	}
	$(MAKE) docs-mermaid-assets
	mdbook serve

# Rebuild when you need a newer engine CLI, OS package, or Rust toolchain
# version — not when sandboard's own source changes, since no sandboard code or
# dependency cache is baked in (cards fetch crates.io/npm live). New sandboxes
# pick this up via --from; existing ones keep the create-time image. One
# image per agent engine (sandbox/Containerfile is multi-stage) — each
# sandbox-<engine> tag only carries that engine's CLI on top of the shared
# UBI9 base + rust toolchain.
# Default engine is podman (OpenShell's usual host driver). Override with
# CONTAINER_ENGINE=docker when needed.
CONTAINER_ENGINE ?= podman
REGISTRY ?= quay.io/sandboard-app
ENGINES := cursor agy claude opencode hermes

sandbox:
	@for e in $(ENGINES); do \
		echo "==> building $(REGISTRY)/sandbox-$$e:latest"; \
		$(CONTAINER_ENGINE) build -f sandbox/Containerfile --target $$e -t $(REGISTRY)/sandbox-$$e:latest . || exit 1; \
	done

sandbox-push: sandbox
	@for e in $(ENGINES); do \
		echo "==> pushing $(REGISTRY)/sandbox-$$e:latest"; \
		$(CONTAINER_ENGINE) push $(REGISTRY)/sandbox-$$e:latest || exit 1; \
	done

# Board image (API + web/dist) for in-cluster / container deploys.
# Distinct from sandbox-* (agent sandboxes). See Containerfile.
image:
	$(CONTAINER_ENGINE) build -f Containerfile -t $(REGISTRY)/sandboard:latest .

image-push: image
	$(CONTAINER_ENGINE) push $(REGISTRY)/sandboard:latest

clean:
	cargo clean
	rm -rf web/dist web/dist-test
