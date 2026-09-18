.DEFAULT_GOAL := help

CARGO ?= cargo

.PHONY: help install fmt fmt-check check test clippy build ci clean

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*##"; printf "Usage: make <target>\n\nTargets:\n"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

install: ## Install the release binary
	$(CARGO) install --path lumen-cli --locked

fmt: ## Format all Rust workspace source files
	$(CARGO) fmt --all

fmt-check: ## Check formatting across the workspace without modifying files
	$(CARGO) fmt --all -- --check

check: ## Type-check all workspace targets with every feature enabled
	$(CARGO) check --workspace --all-targets --all-features --locked

test: ## Run all workspace tests with every feature enabled
	$(CARGO) test --workspace --all-targets --all-features --locked

clippy: ## Run Clippy across the workspace with warnings treated as errors
	$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings

build: ## Build all workspace targets in release mode
	$(CARGO) build --workspace --all-targets --all-features --release --locked

ci: fmt-check check clippy test ## Run the complete workspace CI quality gate

clean: ## Remove Cargo workspace build artifacts
	$(CARGO) clean
