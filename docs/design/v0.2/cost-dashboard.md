# v0.2 — cost dashboard

Status: current for the commands shown; this note is design only. v0.1 has `vowp cost` and the
`cost_today` field on the board; it has no browser view and no budget enforcement.

## Goal

Show where the money went, using the ledger VOWP already writes:

- Spend by day, job, role, provider and model.
- Token totals (input, output, reasoning, cache read/write) next to USD.
- A clear split between **receipt**, **price-table** and **unknown** costs. Unknown is never shown as
  zero and never silently mixed into a total as if it were known.
- Drill-down from a row to the runs and ledger entries behind it.
- A "today" card that matches `vowp board` and `vowp cost --by day`.

The API endpoint reuses `vowp cost --json`; the UI is a table with an optional small chart. A chart is
decoration, not the source of truth.

## Non-goals

- Budget enforcement, hard caps or automatic provider switching. v0.1 reports cost; it does not cap it
  (`plan.md` principle 7).
- Provider invoices, credit balances or a network call to a pricing API.
- Multi-currency support. The ledger is USD.
- A second ledger or a second aggregation rule.

## Existing crates it builds on

- `vowp-store::CostQuery`, `CostGroup` and `Store::cost_totals` — group by job, role, provider, model
  or UTC day; filters for `since`, `until`, project and job.
- `vowp-store::CostRow` — `usd`, `tokens`, `receipt_entries`, `price_table_entries`,
  `unknown_entries` and `entries`.
- `vowp-core::ledger::board::BoardSnapshot` — `cost_today` and its `Cost` value.
- `vowp-core::ledger::price` — the configured price-table estimate used when a harness reports no
  receipt.
- `vowp-cli::cmd::cost` — the exact JSON shape and the `receipt`/`price-table`/`unknown`/`mixed`
  source derivation.
- [`http-api.md`](../v0.3/http-api.md) — the transport.

## Shape

`GET /api/v1/cost?by=day&since=<rfc3339>&until=<rfc3339>&project=<name>&job=<id>` returns the same
document as `vowp cost --json`:

```json
{"by":"day","rows":[{"key":"2026-11-09","runs":12,"usd":1.23,
  "tokens":{"input":1200,"output":300,"reasoning":0,"cache_read":0,"cache_write":0},
  "source":"receipt","unknown_entries":0}],
 "total":{"runs":12,"usd":1.23,
  "tokens":{"input":1200,"output":300,"reasoning":0,"cache_read":0,"cache_write":0},
  "source":"receipt","unknown_entries":0}}
```

The UI renders the rows, marks `source: mixed` and shows the unknown count separately. A row links to
`/api/v1/runs?job=...` or the ledger entries; that drill-down can be a later slice.

## Open questions

1. **Day boundary.** `CostGroup::Day` groups by UTC day. The dashboard should either say "UTC day" or
   accept a `tz` offset; do not silently use the browser's local day and then compare it to the API.
2. **Cached price-table version.** A price-table estimate may change when `vowp.toml` changes. The
   ledger keeps the source but not the price-table version; decide whether to record it.
3. **Reasoning tokens.** They are in `Tokens::reasoning`; show them separately or folded into output?
   The CLI currently shows input/output/cache only.
4. **Budget display.** A user may want a target, but enforcement is v0.3. If a target is shown, label
   it as informational.
5. **Retention.** The ledger is append-only and never deleted; a long-running node may need an archive
   or a date filter default.

## First-issue-sized slice

Add `GET /api/v1/cost` that calls `Store::cost_totals` with the same `CostGroup` and filters as
`vowp cost`, and a read-only table view that shows the rows and the total with the source column. Add a
fixture test with a temporary ledger containing one receipt, one price-table and one unknown cost; the
test must assert that the unknown row is not counted as zero and that the total's source is `mixed`.

No chart, no drill-down and no budget field in the first slice.
