# STUBS.md — loud-stub registry

Per P00 ground-truth rules: anything not yet verified against a live surface lives here,
and any runtime path that uses it emits `tracing::warn!("STUB: ...")`.
Silent stubs are forbidden. Update this file in the prompt that resolves each entry.

| ID | Area | Status | What is unverified / stubbed | Evidence / source | Resolution plan (exit criterion) |
|----|------|--------|------------------------------|-------------------|----------------------------------|
| STUB-01 | Qwen API | UNVERIFIED-PENDING-KEY | Working base URL (`dashscope.aliyuncs.com` vs `dashscope-intl.aliyuncs.com`), exact model string `qwen3.8-max`, `response_format json_object` support, thinking-trace shape | `docs/evidence/p01-connectivity.txt` (401 unauth on both bases); P00 FACT SHEET | P07 first keyed call; client stays behind `Provider` trait + wiremock fixtures until then |
| STUB-02 | Kimi API | UNVERIFIED-PENDING-KEY | Model string (`kimi-k3` family), base URL | same as STUB-01; `api.moonshot.ai` reachable (401 unauth) | P08 first keyed call; fixture-backed until then |
| STUB-03 | CRE tenant chains | PARTIAL | Docs confirm Monad mainnet (CLI >=1.29.0) + testnet (>=1.30.0) support; per-tenant enablement must be confirmed after login | `docs/evidence/p01-cre-cast-checks.txt`; supported-networks-ts.mdx | P14: `cre workflow supported-chains` after `cre login`; simulate locally regardless |
| STUB-04 | Nansen promo | NOT-ADVERTISED | "50% off first 100 settled calls" via `X-Payer-Address` not found in `/.well-known/x402` at P01 | `docs/evidence/p01-nansen-wellknown.body` | P09: send header anyway on initial unpaid request; record first settled call terms |
| STUB-05 | Nansen endpoint schemas | PARTIAL | 402 + rails verified for netflow/holdings/perp-leaderboard/profiler-perp-positions; exact request bodies beyond `{"chains":[...]}` unconfirmed | `docs/evidence/p01-nansen-402*.json`, `p01-nansen-endpoints.txt` | P09: confirm each endpoint's body from its 402/docs at implementation; wiremock fixtures before |
| STUB-06 | Funding-drag math | OPTIONAL | Gateway `Position` exposes entry funding sum (`efs`) but current funding sum must be joined from `funding@` stream; `funding_drag_estimate` stays optional | `vendor/api-docs/types.md` (Position, FundingEvent) | P04: implement only if fixtures carry funding fields; else record here and skip |
| STUB-07 | SDK local test env | OPTIONAL | `perpl-sdk` `testing` feature needs a custom Monad Anvil fork (category-labs/foundry v1.5.0-monad.0.2.0) — not installed | `vendor/dex-sdk/README.md` | Only needed if we run SDK tests locally; we use `default-features = false` and our own fixtures |
| STUB-08 | Docker session note | ENV | luna is in group `docker` but the running session lacks it; use `sudo docker` or `sg` re-login | `docs/evidence/p01-recon-toolchain.txt` | Non-blocking; resolves on next login |

| STUB-09 | P03 live run | PENDING-KEY | `read-positions` live proof + `scripts/record-fixtures.sh` recording require a testnet API key (SETUP-MANUAL step 3); no live frames captured yet | `SPEC.md` §1/§5 | After key: run read-positions on testnet, record `tests/fixtures/perpl/session-*.jsonl`, capture evidence |
| STUB-10 | min_size mapping | PARTIAL | Venue minimum order size is not exposed in `/v1/pub/context` (`min_posting_amount` is collateral-denominated and currently `0`); mapped as `Decimal::ZERO`. P05 implemented the min-size clamp in `reduce_by_fraction` (unit + adversarial tested with synthetic values); dormant on live data until the venue exposes a real value | `SPEC.md` §3.3 / P05 suites | Revisit when the venue exposes min size (or calibrate from a rejected order) |

| STUB-11 | funding drag | SKIPPED (optional) | `funding_drag_estimate` not modeled: core `Position` carries no funding fields (`efs`/`fnd` not mapped); P04 marks it optional | SPEC-P04 §2 / P04 wave reports | Revisit if funding enriches risk (needs type extension + fixtures) |
| STUB-12 | P05 live reduce | PENDING-KEY | The testnet acceptance leg (`test-execution` in TESTNET mode: live 25 % reduce + explorer evidence) needs the testnet API key + seeded position (SETUP-MANUAL steps 2-3; same blocker as STUB-09). DRY_RUN acceptance is green offline | P05 wave reports; `docs/evidence/p05-dryrun-acceptance.txt` | Run TESTNET mode when the key lands; capture screenshot + explorer link into `docs/evidence/p05-live-reduce.png` |
| STUB-13 | P05 reprice retry | TODO | Guard post-verify timeout keeps the inner report and warns; no reprice/retry of a reduce-only marketable order yet (SPEC-P05 §5 marks it out of scope) | SPEC-P05 §5 | P06+: optional single reprice retry for marketable reduces |
## Resolved
- (none yet)
