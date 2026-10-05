# Perpl fixtures (`tests/fixtures/perpl/`)

Session fixtures in **JSONL** format (one JSON object per line). Format is
frozen in `SPEC.md` §3.4; do not change it without a spec version bump.

```jsonl
{"kind":"rest","path":"/v1/pub/context","resp":{...}}
{"kind":"rest","path":"/v1/trading/positions","resp":{...}}
{"kind":"ws","t_ms":0,"msg":{"mt":9,"d":{"32":{"mrk":271370,"at":{"b":1,"t":1791235369000}}}}}
{"kind":"ws","t_ms":1000,"msg":{"mt":21,"id":7,"fw":true,"ft":0,"lfr":42,"b":"100000000","lb":"0"}}
```

- `rest` lines are replayed by `MockPerpl::snapshot()`/`context()` keyed by path.
- `ws` lines are replayed by `MockPerpl::stream()` with `t_ms` pacing
  (compressed); first `mt` matching the trading socket (19/21) or market-data
  (9) semantics.
- Live recordings are produced by `scripts/record-fixtures.sh` against testnet
  (needs `PERPL_API_KEY`); raw captures land as `session-<date>.jsonl`.
