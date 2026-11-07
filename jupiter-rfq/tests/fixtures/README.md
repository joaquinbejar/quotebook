# Fixtures from Jupiter's RFQ webhook toolkit

Copied from <https://github.com/jup-ag/rfq-webhook-toolkit> at commit
`3aa4fce515d4b5827ee2d48f08041ea706d911cd` (2026-09-11, the newest on `main` when read on
2026-11-07). The toolkit is MIT licensed; its licence is kept beside them in
`LICENSE-rfq-webhook-toolkit`.

| File | Source in the toolkit |
| --- | --- |
| `openapi.json` | `openapi/openapi.json`, unchanged |
| `acceptance.json` | payloads of `tests/suites/acceptance/test.ts`: the exact-in quote request, the unsupported-pair request, the swap's transaction, and the expected `rejectedWithReason` state |

`tests/schema.rs` checks the prototype's wire types against them. The swap transaction carries
the older 3-byte payload (35 bytes of `fill` data), so the prototype's validator refuses it, as
docs/design/jupiter-rfq.md requires 5 bytes.
