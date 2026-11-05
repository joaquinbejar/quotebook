# Test fixtures

## `marks.json`

`metalbook_maker::mark` with `OptfixPricer` on a grid of inputs: the 1 oz XAG capped calls of
strikes 48 to 52, a 1 oz XAG put, a 50 oz XAG capped call, and a 1 oz XAU capped call and put; three
spots, three times to expiry, two vols. `tests/marks.rs` requires `quotebook_demo::marks` to give
the same values exactly.

`metalbook-maker` is `publish = false`, so the values are generated outside this repo, against
metalbook at the commit recorded in the file, which is on optfix 0.2 while this crate is on 0.3:
equal results also show that the two versions price these inputs identically.

To regenerate: extract that commit (`git archive <commit> | tar -x -C <dir>`), make a scratch crate
with `generate_marks.rs.txt` as `src/main.rs` and path dependencies on `metalbook-maker` and
`metalbook-core`, add `rust_decimal`, and run it. Its output is the file's `cases`.

## `devnet_accounts.json`

Public devnet accounts as read on 2026-11-05: a live 1 oz XAG capped call, the quotebook series
paired with it, and both Pyth push feeds. `tests/devnet_accounts.rs` holds the published
`metalbook-core`, `metalbook-sdk` and `pyth-lite` 0.1.0 layouts to them, and takes one reader round
over them from account bytes to a mid.
