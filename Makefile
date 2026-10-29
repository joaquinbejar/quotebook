# Makefile for the quotebook workspace. Targets mirror matchbook's and optfix's; `pre-push`
# follows rules/global_rules.md (fmt check, clippy -D warnings, lint-control, SBF build of the
# program, tests, docs -D warnings).

.PHONY: all
all: fmt-check lint test

# sBPF target version (cargo-build-sbf's default is v3).
SBF_ARCH ?= v3

# Build the workspace
.PHONY: build
build:
	cargo build --workspace

# Build the program (and with it the no_std core) for SBF into target/deploy. Fails if any
# stack frame exceeds 4 KiB: cargo build-sbf reports that only as an "Error: Function ...
# overflows the maximum allowed frame space" line and still exits 0, and the overflow
# silently corrupts locals (rules/global_rules.md). Older toolchains say "Stack offset ...
# exceeded max offset". Cached builds print the line again, so the check holds on rebuilds.
SBF_FRAME_OVERFLOW := maximum allowed frame space|exceeded max offset

.PHONY: program
program:
	@out=$$(cargo build-sbf --manifest-path program/Cargo.toml --arch $(SBF_ARCH) 2>&1); \
	status=$$?; printf '%s\n' "$$out"; \
	if [ $$status -ne 0 ]; then exit $$status; fi; \
	if printf '%s\n' "$$out" | grep -Eq '$(SBF_FRAME_OVERFLOW)'; then \
		echo "program: an SBF stack frame exceeds 4 KiB (see above)"; exit 1; \
	fi

# Measure every instruction's CU in LiteSVM and regenerate docs/benchmarks.md
.PHONY: bench
bench: program
	QUOTEBOOK_BENCH_OUT=$(CURDIR)/docs/benchmarks.md \
	cargo test -p quotebook-program --test cu -- --nocapture

# Run every test, including the LiteSVM program tests (which load target/deploy, so `make
# program` runs first). QUOTEBOOK_PROPTEST_CASES=<n> overrides the property-test case count.
.PHONY: test
test: program
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
pre-push: fmt-check lint lint-control program test doc

.PHONY: clean
clean:
	cargo clean
