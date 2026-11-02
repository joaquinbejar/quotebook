# Verifiable build

Milestone I2 of [`PLAN-improvements.md`](PLAN-improvements.md): anyone can rebuild
`quotebook-program` from a commit, get the same bytes, and compare them with what is deployed.

program-sha256: `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`

program-executable-hash: `fe989e9923369050be9c8e266173656fe24aebe9061c9a7d13f3a86ae22d418c`

The two hashes are different things:
- `program-sha256` is the plain SHA-256 of the artifact.
- `program-executable-hash` is what `solana-verify get-executable-hash` prints for it, and so
  what `get-program-hash` must print for the deployed program. It hashes the bytes with the
  trailing zero bytes trimmed. This artifact ends in 15 zero bytes, which is why the two
  differ.

The `program-sha256` line is the SHA-256 of `target/verifiable/quotebook_program.so` built by
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
ProgramData bytes with the trailing zero bytes trimmed, so it is **not** the SHA-256 of the
`.so`. Compare it with `solana-verify get-executable-hash` of the local file, which trims
the same way. Alternatively, `solana program dump` the program and take the plain SHA-256
of the dump: the dump has exactly the program's length, so its hash equals the `.so`'s.

## Checking the devnet program

```sh
make verifiable                                    # or scripts/verifiable/build.sh <commit>
solana-verify get-executable-hash target/verifiable/quotebook_program.so
solana-verify get-program-hash 3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo \
  --url https://api.devnet.solana.com              # must print the same hash
solana program dump 3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo dump.so --url devnet
shasum -a 256 dump.so                              # must equal program-sha256 above
```

When an official 4.4 image appears, switching to `solana-verify build --base-image` is a
one-line change. Its output must match the hash above before the switch.

## Records

| Commit | Built by | SHA-256 | Notes |
| --- | --- | --- | --- |
| `898c173` | `cargo build-sbf` on macOS (Studio), outside Docker | `d7c2f1de7114751cb05849a981924956bb0e632ae5f849cc9fd62caa7fc721e7` | the first devnet deployment (`devnet.md`). The dump of the deployed program hashed to this value before the upgrade; `get-program-hash` gave `2ab5be6a93327220454b420c4c7e3b4c8ee37f1bbff131847c990ad95fd6c9f9` |
| `898c173` | `scripts/verifiable/build.sh` | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | same code, different bytes: the macOS build is not reproducible |
| `92cdd22` (main) | `scripts/verifiable/build.sh`, twice, on the Studio | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | byte-identical runs. Equal to `898c173`: the program and core code is the same, and the 0.1.1 version bump does not reach the binary |
| `92cdd22` (main) | CI `verifiable` job on a GitHub runner, native amd64 (run 41416573833) | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776` | the same bytes on a second machine |

## Devnet upgrade to the reproducible binary (2026-11-02)

| Item | Value |
| --- | --- |
| Binary | `target/verifiable/quotebook_program.so` from `92cdd22`, 35,032 bytes, SHA-256 `a6a324f0…0776` |
| `solana program deploy` (upgrade, authority `77ie…CPPV`) | `2NDo6wEyNUeVGcxxJ7igHEGpoVmK4HXunov8QjJ8Mu9XvwuED3zG7Xj622JpxydQUwcqWXdahR8TyWjtUrXx2VJg` |
| Last deployed in slot | 517802812 |
| `solana-verify get-program-hash` after | `fe989e9923369050be9c8e266173656fe24aebe9061c9a7d13f3a86ae22d418c`, equal to `get-executable-hash` of the binary |
| `solana program dump` SHA-256 after | `a6a324f0a3c624bd20706ec489e99a9c5b98fdba6627258c0d9c7b186f310776`, equal to `program-sha256` |

The program and core code is unchanged since `898c173`, so this upgrade changes no
behaviour. Every account, the program id and outstanding quotes are unaffected. The
temporary buffer account was consumed by the upgrade (`solana program show --buffers` lists
none).

Smoke test after the upgrade: a `settle` on series 0 (`6XjcD…pGh7`) filled 1 lot at 2,565 ticks
from maker `5btd…mWW5` to taker `3ZBj…sqLs`, using 8,595 CU, the same as on the macOS build:
`a9NdJwwzp3X4MnxDbYydsv2bnD6q9GLSDXKXfTrcMb4R5mKuT6XEpmB7mpGTUaVKjiETyRcwXhQJfvMX89cGgho`.
The maker service was stopped by PID afterwards.
