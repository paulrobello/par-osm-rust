# par-osm-rust Makefile
#
# Standard target set (build, test, lint, fmt, typecheck, checkall, clean).
# Commands mirror what CI runs in .github/workflows/ci.yml.

.PHONY: all build build-release test lint fmt fmt-check typecheck check checkall bench pre-commit clean

# Default target
all: build

# --- Build -----------------------------------------------------------------
build:
	cargo build

build-release:
	cargo build --release

# --- Verification ----------------------------------------------------------
test:
	cargo test --all-features

lint:
	cargo clippy --all-targets --all-features -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

# `typecheck` maps to `cargo check` per the standard target set.
typecheck:
	cargo check --all-targets

check: fmt-check lint

# Full gate: formatting, lint (deny warnings), type-check, and tests.
checkall: fmt-check lint typecheck test

# Run `criterion` benchmarks (perf-critical parse/dedupe/write paths).
bench:
	cargo bench

# --- Tooling ----------------------------------------------------------------
# Run every pre-commit hook (secret scanning + hygiene + language checks).
pre-commit:
	pre-commit run --all-files

# --- Cleanup ---------------------------------------------------------------
clean:
	cargo clean
