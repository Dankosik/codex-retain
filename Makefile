PYTHON ?= python3
CARGO ?= cargo

.NOTPARALLEL:

.PHONY: help run build fmt lint test check verify audit template-check template-smoke init

help:
	@echo "run             Run the CLI (ARGS='stats input.txt')"
	@echo "build           Build the optimized executable"
	@echo "check           Formatting, Clippy, and Rust tests"
	@echo "verify          check plus template/maintenance validation"
	@echo "audit           Dependency advisories, licenses, and sources (cargo-deny)"
	@echo "template-smoke  Initialize and test a disposable consumer"
	@echo "init            Set identity (NAME=my-tool REPOSITORY=https://github.com/me/my-tool)"

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

template-check:
	$(PYTHON) scripts/check_template.py
	$(PYTHON) -m unittest discover -s scripts/tests

verify: check template-check

audit:
	$(CARGO) deny check

template-smoke:
	$(PYTHON) scripts/check_template.py --consumer

init:
	$(PYTHON) scripts/init.py --name "$(NAME)" --repository "$(REPOSITORY)"
