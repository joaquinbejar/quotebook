# Makefile for the quotebook workspace. Targets mirror matchbook's and optfix's; `pre-push`
# follows rules/global_rules.md (fmt check, clippy -D warnings, lint-control, SBF build, tests,
# docs -D warnings).

.PHONY: all
all: fmt-check lint test

# sBPF target version (cargo-build-sbf's default is v3).
SBF_ARCH ?= v3

# Build the workspace
.PHONY: build
build:
	cargo build --workspace

# Build the no_std core for SBF, proving it compiles for the on-chain target. The program
# crate joins this target in M2.
.PHONY: sbf
sbf:
	cargo build-sbf --manifest-path quotebook-core/Cargo.toml --arch $(SBF_ARCH)

# Run every test. QUOTEBOOK_PROPTEST_CASES=<n> overrides the property-test case count.
.PHONY: test
test:
	cargo test --workspace --all-features

# Format the code
.PHONY: fmt
fmt:
	cargo +stable fmt --all

# Check formatting
.PHONY: fmt-check
fmt-check:
	cargo +stable fmt --all --check

# Clippy with every warning denied
.PHONY: lint
lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

# Failing control: clippy must reject every banned macro in lint-control's library code, on
# each probe line with the exact definition path (see lint-control/check.sh).
.PHONY: lint-control
lint-control:
	@sh lint-control/check.sh

# Rustdoc with every warning denied
.PHONY: doc
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Pre-push checks: must pass with zero warnings
.PHONY: pre-push
pre-push: fmt-check lint lint-control sbf test doc

.PHONY: clean
clean:
	cargo clean
