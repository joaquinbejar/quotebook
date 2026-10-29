Rules for writing Rust in the Solana projects optfix, quotebook, varswap and metalbook:
on-chain programs (Pinocchio), `no_std` engine and math crates, and their off-chain SDKs and
services. All code, comments, docs, commit messages and PR descriptions in English. No dashes
as punctuation in prose: use `:` or `;`.

The same file lives in all four repos; keep them identical. Repo specifics go in `CLAUDE.md`.

---

## Panics: Never Outside Tests

A panic in a Solana program aborts the transaction with an opaque error, and in a library it
takes the decision away from the caller. Production code never panics.

- ZERO `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `assert!`, `assert_eq!`,
  `.unwrap()`, `.expect()`, unchecked `[]` indexing, slicing that can go out of range,
  integer division or remainder by a value that can be zero, or unchecked arithmetic.
- `debug_assert!` is also out in on-chain and `no_std` crates: behavior must not differ
  between debug and release.
- Every fallible path returns a typed error. Invariants the type system cannot encode are
  checked and return an error variant (for example `InvariantViolated`), never assert.
- `overflow-checks = true` stays in the release profile as a last-resort net; the code must
  never rely on it, because hitting it is a panic.
- Tests, benches and test harnesses (`#[cfg(test)]`, `tests/`, `bench/`) may panic, unwrap
  and assert; they start with an explicit `#![allow(...)]` like matchbook's test files,
  including `clippy::float_arithmetic` where they compare against `f64` references.
- Enforced by lints in `[workspace.lints]`:
  ```toml
  [workspace.lints.rust]
  unsafe_code = "deny"
  missing_docs = "deny"

  [workspace.lints.clippy]
  panic = "deny"
  unwrap_used = "deny"
  expect_used = "deny"
  unreachable = "deny"
  todo = "deny"
  unimplemented = "deny"
  indexing_slicing = "deny"
  arithmetic_side_effects = "deny"
  integer_division = "deny"
  float_arithmetic = "deny"          # on-chain and no_std crates
  panic_in_result_fn = "deny"
  manual_assert = "deny"
  ```

---

## Dependencies

- Always the latest stable version available on crates.io at the time the dependency is
  added or touched. Check `https://crates.io/api/v1/crates/<name>` (`max_stable_version`),
  never rely on memory, and cite the check in the PR.
- Requirements written as `MAJOR.MINOR` (`pinocchio = "0.12"`), never `X.Y.Z` and never `*`.
- When the latest versions of two crates are incompatible (for example a companion crate
  still built for an older core crate), stop and record the conflict in `docs/PLAN.md` with
  the options; do not silently downgrade.
- No new dependency without approval. On-chain crates keep the dependency tree minimal: no
  `std`, no allocator, no Anchor, no `serde`.
- Workspace-level `[workspace.dependencies]`, members use `{ workspace = true }`.
- Unpublished sibling projects are consumed as git dependencies pinned by `rev`, never by
  path, until they are on crates.io.

---

## Compiler Attributes

### #[must_use]
- All pure functions (pricing, greeks, conversions, validation, PDA derivation, encoding).
- Every function returning `Result` whose outcome the caller must handle.

### #[inline] / #[inline(always)] / #[inline(never)]
- `#[inline]`: newtype accessors and conversions, small fixed-point helpers, account
  accessors.
- `#[inline(always)]`: only kernels proven hot by a CU benchmark (`mul_wide`, polynomial
  evaluation). Re-measure CU when adding or removing it; on SBF inlining changes both CU and
  stack usage.
- `#[inline(never)]`: error construction, logging, large instruction handlers (keeps SBF
  stack frames under the 4 KiB limit).

### #[cold]
- Error construction helpers and rejection paths.

### #[repr]
- `#[repr(C)]` + `bytemuck::Pod` + `Zeroable` for every account and every signed or
  serialized payload. No implicit padding: padding is an explicit `_pad: [u8; N]` field.
- `#[repr(transparent)]` for single-field newtypes.
- `#[repr(u8)]` / `#[repr(u32)]` for enums with stable discriminants. Enums are not `Pod`:
  store them as the integer and validate on load.
- Check alignment on both host and SBF (`i128` is 16-aligned on x86_64 hosts, account data is
  only guaranteed 8-aligned). Use byte-array storage forms where they differ.

### #[derive] in this order
```
Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default
```
Derive only what is meaningful. No `Default` where zero is not a valid value.

---

## Type Safety

- Never raw primitives for domain quantities: `Spot`, `Strike`, `Volatility`, `Years`,
  `BaseLots`, `QuoteAtoms`, `Nonce`, `UnixTimestamp`, `Variance`. The compiler must reject a
  volatility passed as a strike.
- Constructors `new()` / `try_new()` returning `Result`; fields private; values read from
  account data go through a validating constructor, never a raw cast.
- On-chain and `no_std` code: integers and the optfix `Fixed` type only. No `f32` / `f64`.
- Off-chain code may use `rust_decimal::Decimal` (as OptionStratLib and market-maker-rs do)
  and converts to on-chain integers at one boundary with an explicit rounding direction.
- `NonZero*` where zero is structurally invalid.

---

## Arithmetic and Rounding

- `checked_*` everywhere; widen to `u128` / 256-bit for products. Overflow is a typed error.
- NEVER `saturating_*` or `wrapping_*` on amounts, prices or anything financial.
- Every lossy operation states its rounding direction in the code (`mul_down`, `div_up`,
  a `Rounding` argument) and in a comment: toward the protocol or toward the user. A library
  that cannot know which side is the protocol takes a `Rounding` argument or rounds to
  nearest and publishes an error bound.
- Conservation is tested: what leaves a vault never exceeds what entered it, and dust has a
  documented owner.

---

## Error Handling

- On-chain: one hand-written error enum per program, `#[repr(u32)]`, codes from 100 (matchbook
  convention), `impl From<Error> for ProgramError`. Library error enums (`PricingError`) are
  `#[repr(u8)]` and mapped into program codes. No `String` payloads.
- Off-chain: `thiserror` enums per module; no `anyhow` in libraries.
- `.get()` / `.get_mut()` / `split_at_checked` for slices; `.ok_or(Error::X)` for `Option`.

---

## On-Chain Programs

- Pinocchio, not Anchor. `no_std`, `no_allocator!()`, `nostd_panic_handler!()`.
- Every account an instruction receives is validated before use: signer, writable, owner
  program, discriminator/tag, PDA re-derived from seeds with the stored bump, expected mint,
  expected vault, expected token program, sysvar address.
- State is updated before any CPI.
- Clock from the syscall or the verified sysvar, never from instruction data.
- Every validation rule has a LiteSVM negative test with a forged account (Mollusk allowed for
  instruction-level unit tests).
- Every instruction has a measured CU cost in `docs/benchmarks.md`, regenerated by
  `make bench`; a PR that changes a handler updates the table.
- Logging on-chain only for events clients need (`pinocchio::log`), kept minimal: logs cost CU.

---

## Off-Chain Code (SDKs, services, tools)

- `tracing` for logging; never `println!`, `eprintln!`, `dbg!` or `log` (CLI output excepted).
  Libraries never install a global subscriber.
- Async only behind a feature or in service binaries; `tokio::sync` primitives; no blocking
  I/O in async without `spawn_blocking`.
- Never log keys, seeds or signatures of unsent transactions.

---

## Documentation

- Every `pub` item has `///` docs; `# Errors` on fallible functions. No `# Panics` sections:
  nothing panics.
- Units always stated: "per year", "atoms", "lots", "bps", "seconds".
- Each public math function documents its maximum error and its CU cost.
- Any `unsafe` (target zero; `unsafe_code = "deny"`) needs a `// SAFETY:` comment and an
  explicit `#[allow]` reviewed in the PR.

---

## Testing

- Unit tests in the same file; integration and LiteSVM tests under `tests/`.
- Happy path and every documented error case. Every account-validation rule has a forged-
  account test.
- Property tests (proptest) for math and state machines, with a reference model.
- Numerical code is checked against OptionStratLib or mpmath/libm references with the
  documented bound; unit mismatches are rescaled explicitly, never absorbed into tolerances.
- Names: `test_<unit>_<scenario>_<expected>`.
- Deterministic seeds.

---

## Pre-Submission Checklist

All must pass; failing any means not ready:

- `make pre-push` (fmt check, clippy `-D warnings` on all targets and features, tests,
  `cargo build-sbf` for programs, docs with `-D warnings`), zero warnings.
- No panic paths in production code (lints above clean).
- Dependencies at the latest stable version, written `MAJOR.MINOR`.
- CU table updated for changed instructions; error-bound table updated for changed math.
- Negative tests for every new account-validation rule.
- `docs/PLAN.md` updated if a decision changed.

---

## DO NOT

- Panic in production code, in any form listed above.
- Use floating point on-chain or in `no_std` crates.
- Use `saturating_*` / `wrapping_*` on financial values.
- Trust instruction data for anything an account or sysvar can prove.
- Add or downgrade dependencies without approval; pin anything below latest stable silently.
- Push or open PRs without Joaquin's approval.
