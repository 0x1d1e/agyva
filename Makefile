CARGO ?= cargo
PYTHON ?= python3
SHELL := /bin/sh

.PHONY: all fmt fmt-check check lint clippy test build release deny links ci clean

all: ci

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

check:
	$(CARGO) check --all-targets --locked

clippy:
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings

lint: fmt-check clippy

test:
	$(CARGO) test --all-targets --locked

build:
	$(CARGO) build --release --locked

release:
	scripts/release.sh

deny:
	$(CARGO) deny check

links:
	$(PYTHON) -I scripts/check-links.py

ci: fmt-check check lint test build links
