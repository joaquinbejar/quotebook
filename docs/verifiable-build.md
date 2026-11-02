# Verifiable build

Milestone I2 of [`PLAN-improvements.md`](PLAN-improvements.md): anyone can rebuild
`quotebook-program` from a commit, get the same bytes, and compare them with what is deployed.

program-sha256: `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`

That line is the SHA-256 of `target/verifiable/quotebook_program.so` built by
`make verifiable` from the current tree. CI's `verifiable` job rebuilds it and fails if the
hash differs (`make verifiable-check`), so a change to the program updates this line in the
same PR, as `bench` does for `docs/benchmarks.md`.

## How it is built

`scripts/verifiable/build.sh [<commit>]` (`make verifiable` builds `HEAD`):

1. builds the image `scripts/verifiable/Dockerfile`:
   - base `ubuntu:24.04`, pinned by digest;
   - host Rust 1.99.1;
   - the Agave CLI 4.4.0 release tarball (cargo-build-sbf 4.5.0);
   - platform-tools v1.57 (SBF Rust 1.95.0).
   These are the CI pins (`.github/workflows/ci.yml`). Both archives are checked against
   their SHA-256, and the image build fails unless every tool reports its pinned version.
2. streams the committed tree (`git archive <commit>`) into the container at `/work`. So
   uncommitted changes and the checkout's path cannot reach the binary.
3. runs `cargo build-sbf --manifest-path program/Cargo.toml --arch v3 -- --locked` and
   writes `target/verifiable/quotebook_program.so`;
4. prints the commit and the SHA-256.

The image is `linux/amd64`: native on CI runners, emulated on Apple silicon (about 5 minutes
on the Studio).

## Why not `solana-verify build`

`solana-verify` (0.5.2 on crates.io, checked 2026-11-02) builds inside
`solanafoundation/solana-verifiable-build:<agave version>`. Docker Hub has tags up to
`4.3.0`, none for 4.4 (checked 2026-11-02). Building with 4.3 would not be the toolchain CI
and the bench use. So the image is our own, from the same verified archives.

`solana-verify get-program-hash` is still used to read the deployed program. It hashes the
ProgramData bytes without the trailing zero padding, which is directly comparable with the
SHA-256 of the `.so`.

When an official 4.4 image appears, switching to `solana-verify build --base-image` is a
one-line change. Its output must match the hash above before the switch.

## Records

| Commit | Built by | SHA-256 | Notes |
| --- | --- | --- | --- |
| `898c173` | `cargo build-sbf` on macOS (Studio), outside Docker | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` | the first devnet deployment (`devnet.md`) |
| `898c173` | `scripts/verifiable/build.sh` | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | same code, different bytes: the macOS build is not reproducible |
| `92cdd22` (main) | `scripts/verifiable/build.sh`, twice, on the Studio | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | byte-identical runs. Equal to `898c173`: the program and core code is the same, and the 0.1.1 version bump does not reach the binary |
