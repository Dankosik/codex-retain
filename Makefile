PYTHON ?= python3
CARGO ?= cargo

.NOTPARALLEL:

.PHONY: help run build fmt lint test check verify audit maintenance-check

help:
	@echo "run             Run the CLI (ARGS='--help')"
	@echo "build           Build the optimized executable"
	@echo "check           Formatting, Clippy, and Rust tests"
	@echo "verify          check plus release/skill maintenance tests"
	@echo "audit           Dependency advisories, licenses, and sources (cargo-deny)"
	@echo "maintenance-check  Test release archives and vendored skill maintenance"

run:
	$(CARGO) run --locked -- $(ARGS)

build:
	$(CARGO) build --locked --release

fmt:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --locked --all-targets -- -D warnings

test:
	$(CARGO) test --locked --all-targets
	$(CARGO) test --locked --doc

check: fmt lint test

maintenance-check:
	$(PYTHON) -m unittest discover -s scripts/tests -p test_release.py
	$(PYTHON) -m unittest discover -s scripts/tests -p test_sync_skills.py

verify: check maintenance-check

audit:
	$(CARGO) deny check
