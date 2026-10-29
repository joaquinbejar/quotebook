# quotebook

RFQ on Solana: makers sign `Quote`s off-chain (ed25519), takers settle with the ed25519
precompile plus `settle`, verified through the instructions sysvar. The precompile instruction must be
self-contained (all instruction indexes `0xFFFF`) and the program reads the quote from its data.
Crates: `quotebook-core` (no_std shared types and encoding), `program`, `sdk`, `maker` (std service
using `~/Repos/Local/market-maker-rs`). Replay protection: per-maker 16,384-bit nonce ring whose
`window_base` is also the cancel-all watermark; per-maker delegate PDA.

## Rules

`rules/global_rules.md` is mandatory (shared by the four Solana repos, keep it identical):
no panics outside tests, latest stable dependencies, checked arithmetic, explicit rounding.

## Conventions shared with matchbook

- Style reference: `~/Repos/Local/matchbook` (read only). Copy its workspace layout, `rust-toolchain.toml`
  (stable), `[workspace.lints]`, release profile (`overflow-checks = true`), Makefile targets and docs layout.
- All code, comments, docs and commit messages in English. No dashes as punctuation in prose; use `:` or `;`.
- On-chain code: Rust, Pinocchio (not Anchor), `no_std`, no allocator, `bytemuck` zero-copy accounts.
  No floating point and no unchecked arithmetic; every rounding is explicit and its direction
  (toward the protocol or toward the user) is documented at the call site.
- Newtypes for every quantity so the compiler rejects mixing them. No panics of any kind outside
  tests (`panic!`, `unwrap`, `expect`, `assert!`, indexing, unchecked arithmetic).
- Tests: LiteSVM for programs (Mollusk allowed for instruction-level unit tests), proptest for math.
  Every account-validation rule (signer, owner, PDA seeds, mint, vault) has a negative test with a
  forged account.
- Every instruction has a measured CU cost in `docs/benchmarks.md`, produced by `make bench`.
- Dependencies at the latest stable version on crates.io, written `MAJOR.MINOR`. Verify versions
  and APIs on crates.io or docs.rs before relying on them and cite what was checked.
- `make pre-push` must pass with zero warnings before any push.

## Git

- One feature branch per milestone (`feat/m<N>-<name>`), small commits, PR to `main`.
- Push, opening PRs and merging are always authorized (Joaquin, 2026-10-29); do not ask for
  confirmation. Never ask Joaquin anything: questions go to the SOLANA-PROJECTS coordinator
  (`joaquin/s-1836f1d6a089` on the ai-crew-sync bus), which decides or escalates. Merge when the
  coordinator says so, normally after codex's review is addressed. Commits are signed (taunais
  identity via includeIf).
- Worktrees go in `/Users/joaquin/tmp/worktrees/<repo>/<name>`.

## Plan

`docs/PLAN.md` is the source of truth for the domain model, accounts, milestones and open questions.
Update it when a decision changes; record decisions with the numbers that drove them.
