# Makefile for the quotebook workspace. Targets mirror matchbook's and optfix's; `pre-push`
# follows rules/global_rules.md (fmt check, clippy -D warnings, lint-control, SBF build of the
# program, tests, docs -D warnings).

.PHONY: all
all: fmt-check lint test

# sBPF target version (cargo-build-sbf's default is v3).
SBF_ARCH ?= v3

# Host toolchain for the explicit `+toolchain` calls (rustfmt): `stable` locally, the exact
# pin in CI (.github/workflows/ci.yml), where RUSTUP_TOOLCHAIN covers the plain `cargo` calls
# but cannot override an explicit `+stable`.
RUST_TOOLCHAIN ?= stable
# Extra flags for every cargo command; CI sets `--locked`.
CARGO_FLAGS ?=
export CARGO_FLAGS

# Build the workspace
.PHONY: build
build:
	cargo build $(CARGO_FLAGS) --workspace

# Build the program (and with it the no_std core) for SBF into target/deploy.
# cargo-build-sbf's exit status is kept (no pipe), and scripts/check-sbf-log.sh (shared with
# optfix) fails the build on a stack frame above 4 KiB, which cargo-build-sbf only warns
# about. Cargo prints the line again on cached builds, so a no-op rebuild still fails.
.PHONY: program
program:
	@mkdir -p target
	@cargo build-sbf --manifest-path program/Cargo.toml --arch $(SBF_ARCH) -- $(CARGO_FLAGS) \
		> target/build-sbf.log 2>&1; status=$$?; cat target/build-sbf.log; \
		if [ $$status -ne 0 ]; then echo "program: cargo-build-sbf failed ($$status)"; exit $$status; fi
	@sh scripts/check-sbf-log.sh target/build-sbf.log

# Failing control for `program`: the log check must reject a frame overflow in both wordings,
# and a failing cargo-build-sbf must fail the target.
.PHONY: program-control
program-control:
	@mkdir -p target
	@printf '%s\n' 'Error: Function entrypoint overflows the maximum allowed frame space by accessing an offset 64 bytes greater than the maximum of 4096.' \
		> target/sbf-control.log
	@if sh scripts/check-sbf-log.sh target/sbf-control.log > /dev/null; then \
		echo "program-control: the log check accepted a frame overflow"; exit 1; fi
	@printf '%s\n' 'Error: Function _ZN4main Stack offset of 4160 exceeded max offset of 4096 by 64 bytes' \
		> target/sbf-control.log
	@if sh scripts/check-sbf-log.sh target/sbf-control.log > /dev/null; then \
		echo "program-control: the log check accepted the older wording"; exit 1; fi
	@if $(MAKE) --no-print-directory program SBF_ARCH=no-such-arch > /dev/null 2>&1; then \
		echo "program-control: make program succeeded with a failing cargo-build-sbf"; exit 1; fi
	@echo "program-control: a frame overflow and a failing cargo-build-sbf both fail make program"

# The test-only probe program (test-programs/probe; I7 event tests: a spoofing emitter and
# settle by CPI), built for SBF next to the program. Never deployed.
.PHONY: test-programs
test-programs:
	@mkdir -p target
	@cargo build-sbf --manifest-path test-programs/probe/Cargo.toml --arch $(SBF_ARCH) -- $(CARGO_FLAGS) \
		> target/build-sbf-probe.log 2>&1; status=$$?; cat target/build-sbf-probe.log; \
		if [ $$status -ne 0 ]; then echo "test-programs: cargo-build-sbf failed ($$status)"; exit $$status; fi
	@sh scripts/check-sbf-log.sh target/build-sbf-probe.log

# The TypeScript SDK (ts/; I8): install exactly the locked versions, type-check, run the
# vector tests, and require that the TypeScript-built settle transaction
# (ts/vectors/ts-settle.txt, replayed by sdk/tests/ts_settle.rs) is the committed one.
.PHONY: ts-check
ts-check:
	@cd ts && npm ci --no-audit --no-fund --silent
	@cd ts && npx tsc --noEmit
	@cd ts && node --test test/
	@cd ts && node scripts/emit-settle.ts > ../target/ts-settle.txt
	@diff -u ts/vectors/ts-settle.txt target/ts-settle.txt \
		|| { echo "ts-check: ts/vectors/ts-settle.txt is stale; run 'cd ts && node scripts/emit-settle.ts > vectors/ts-settle.txt'"; exit 1; }

# The vectors the TypeScript SDK is checked against must be what the Rust crates generate now.
.PHONY: vectors-check
vectors-check:
	@mkdir -p target
	@cargo run -q $(CARGO_FLAGS) -p quotebook-sdk --example vectors > target/vectors.json
	@diff -u ts/vectors/vectors.json target/vectors.json \
		|| { echo "vectors-check: ts/vectors/vectors.json is stale; run 'cargo run -p quotebook-sdk --example vectors > ts/vectors/vectors.json'"; exit 1; }

# Measure every instruction's CU in LiteSVM and regenerate docs/benchmarks.md
.PHONY: bench
bench: program
	QUOTEBOOK_BENCH_OUT=$(CURDIR)/docs/benchmarks.md \
	cargo test $(CARGO_FLAGS) -p quotebook-program --test cu -- --nocapture

# Reproducible build of the program in Docker (scripts/verifiable/Dockerfile, the CI pins),
# from the committed tree only, into target/verifiable/quotebook_program.so.
.PHONY: verifiable
verifiable:
	@sh scripts/verifiable/build.sh HEAD

# The reproducible build must hash to the `program-sha256` line of docs/verifiable-build.md,
# so a change to the program updates the recorded hash in the same PR (as `bench` does for
# docs/benchmarks.md). Not part of pre-push: it needs Docker; CI runs it.
.PHONY: verifiable-check
verifiable-check: verifiable
	@want=$$(sed -n 's/^program-sha256: `\([0-9a-f]*\)`.*/\1/p' docs/verifiable-build.md); \
	if command -v sha256sum > /dev/null; then sum="sha256sum"; else sum="shasum -a 256"; fi; \
	got=$$($$sum target/verifiable/quotebook_program.so | cut -d' ' -f1); \
	if [ -z "$$want" ] || [ "$$want" != "$$got" ]; then \
		echo "verifiable-check: built $$got, docs/verifiable-build.md records '$$want'"; exit 1; fi; \
	echo "verifiable-check: $$got matches docs/verifiable-build.md"

# Run every test, including the LiteSVM program tests (which load target/deploy, so `make
# program` runs first). QUOTEBOOK_PROPTEST_CASES=<n> overrides the property-test case count.
.PHONY: test
test: program test-programs
	cargo test $(CARGO_FLAGS) --workspace --all-features

# Format the code
.PHONY: fmt
fmt:
	cargo +$(RUST_TOOLCHAIN) fmt --all

# Check formatting
.PHONY: fmt-check
fmt-check:
	cargo +$(RUST_TOOLCHAIN) fmt --all --check

# Clippy with every warning denied
.PHONY: lint
lint:
	cargo clippy $(CARGO_FLAGS) --workspace --all-targets --all-features -- -D warnings

# Failing control: clippy must reject every banned macro in lint-control's library code, on
# each probe line with the exact definition path (see lint-control/check.sh).
.PHONY: lint-control
lint-control:
	@sh lint-control/check.sh

# Rustdoc with every warning denied
.PHONY: doc
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc $(CARGO_FLAGS) --workspace --no-deps --all-features

# Pre-push checks: must pass with zero warnings
.PHONY: pre-push
pre-push: fmt-check lint lint-control program-control program test doc vectors-check ts-check

.PHONY: clean
clean:
	cargo clean
