# Sentinel — paste-ready bounty answers (Monad Metropolis)

Generated 2026-10-06 from the frozen fact sheet (SPEC-P17 §1). Seven self-contained blocks, each ≤200 words (heading through Evidence line), ready to paste into the corresponding bounty form. The platform's exact form-question wording is not stored in this repo, so each block is written as a direct answer to that bounty's core question. Every number traces to an evidence file named on the block's own Evidence line; PENDING items are stated inline and collected at the end.

Map (answer → bounty as listed on the Metropolis platform):
1. Perpl — Best use of Perpl's API · 2. Perpl — Best Analytics / Risk Tool · 3. Nansen — Best use of Nansen · 4. Alibaba Cloud — Best Builds with Qwen 3.8 Max · 5. Kimi — Best Builds Powered by KIMI · 6. Chainlink — Best workflow with CRE · 7. Envio — Best Use of Envio

---

## 1. Perpl — Best use of Perpl's API

Sentinel's venue integration is a from-scratch Rust client (`crates/sentinel/src/perpl/`) that speaks Perpl's gateway API directly: Ed25519 request signing with the exact canonical strings (X-API-Key/Timestamp/Nonce/Signature), typed REST reads (`/v1/pub/context`, wallet, positions, orders, fills, account history), and both WebSocket feeds (market-data + trading with `mt:29` sign-in, heartbeat-`sn` continuity, staleness detection, reconnect with backoff). Defensive orders go out as reduce-only CloseLong/CloseShort requests through the batch endpoint, using `rq` as the idempotency key — a same-`rq` re-send is at-most-once. The executor is acceptance-tested in DRY_RUN: a reduce of 2.500 on a 10-unit position fills at 2710.98630, and the repeated submit is suppressed as a duplicate (key 32:reduce:250). Why it matters: automated protection only counts if it can actually place the defensive reduce on the venue. The live testnet reduce remains PENDING-KEY (STUB-12); everything above is offline/DRY_RUN-verified.

Evidence: docs/evidence/p05-dryrun-acceptance.txt; crates/sentinel/src/perpl/auth.rs; crates/sentinel/src/perpl/rest.rs; crates/sentinel/src/perpl/ws.rs

---

## 2. Perpl — Best Analytics / Risk Tool

Sentinel is a non-custodial risk tool for Perpl's isolated-margin perpetuals: it reconstructs per-position liquidation distance from Perpl's leverage semantics (the gateway exposes no liquidation price), classifies positions into Green/Yellow/Orange/Red tiers, and runs a deterministic reflex engine that de-risks on breach — no LLM on the reflex path. Measured: classification costs 0.271 µs/iteration (10,000 iterations in 2.707756 ms; 2,200 intents), and the frozen backtest reads: "Across 14 scenarios representing $60163.365 notional, Sentinel preserved $5972.13546111693125 (9.93%); baseline liquidations: 11 -> with Sentinel: 0". The live dashboard pass includes the audit verifier (36 entries, no broken link) and a kill-switch round-trip. Why it matters: in isolated margin one position can liquidate while the account still shows free balance; Sentinel prices that per-position risk continuously and acts before liquidation. The live testnet reduce is PENDING-KEY (STUB-12).

Evidence: docs/backtest-report.md; docs/evidence/p04-skill-verification.txt; crates/sentinel-core/src/risk.rs; docs/evidence/p15-validate.txt

---

## 3. Nansen — Best use of Nansen

Sentinel buys Nansen smart-money data per call over x402 v2 — USDC on Monad, no API key. The client parses the HTTP 402 challenge (body first, then the `payment-required` header), selects the Monad rail (`eip155:143`), signs the EIP-3009 TransferWithAuthorization via EIP-712 — digest 0xab74a1066d05d2a495f2d5938ba4db686a7039cbf63cd6e456a2f11c641c0951, pinned from an independent ethers 6.17 oracle run (alloy recovery re-checks the signer) — then retries with `PAYMENT-SIGNATURE` and `X-Payer-Address`. Recorded 402 challenges for smart-money netflow, holdings, perp-leaderboard and perp-positions pin the wire shapes; fetched data lands in the strategy prompt as a SmartMoneyContext block, is budget-capped and cached, and a 402/timeout can never stall the reflex path (cached data, or the literal "smart-money data unavailable — decide without it", is used instead). The live free leg is green: 8 rails parsed, Monad rail selected, check rc=0. The first paid purchase remains PENDING-WALLET (STUB-16).

Evidence: docs/evidence/p09-x402-check.txt; crates/sentinel/src/nansen/x402.rs; crates/sentinel/tests/nansen_adversarial.rs; tests/fixtures/nansen/eip712_oracle.js

---

## 4. Alibaba Cloud — Best Builds with Qwen 3.8 Max

Qwen 3.8 Max is Sentinel's primary strategic brain: a schema-validated consult path that turns the account snapshot, policy envelope, and Nansen context into strict JSON decisions (HOLD/REDUCE/CLOSE/ADD_COLLATERAL/ESCALATE). The provider requests `response_format: json_object` with a token budget sized for Qwen's thinking trace; a tolerant parser strips the thinking preamble, extracts the first balanced JSON object, and makes a repair call before failing. A grounding check enforces that every number in the model's reason appears in the prompt, and a confidence floor downgrades weak answers to ESCALATE — the deterministic core always disposes. Offline eval scoreboard, quoted verbatim: "action-class accuracy (core): 12/12"; "schema validity: 14/14"; "injection schema validity: 2/2"; "grounding: 14/14". Why it matters: risk decisions benefit from model judgment, but never unchecked output — Qwen's answers must satisfy a JSON contract, grounding, and injection tests. The first keyed live run remains PENDING-KEY (STUB-01); the eval harness ships with the full mock proof.

Evidence: docs/evidence/p07-brain-eval-mock.txt; crates/sentinel/src/brain/providers.rs; crates/sentinel/src/brain/parser.rs; docs/FACTS.md

---

## 5. Kimi — Best Builds Powered by KIMI

Kimi is the fallback provider in Sentinel's resilient reasoning chain [Qwen → Kimi]: same decision schema and engine, with per-slot circuit breakers (closed/open/half-open) so a failing primary degrades instead of stalling the guardian. Failover is proven offline: with FORCE_PROVIDER_FAIL=qwen, the full brain_eval scoreboard passes 14/14 rows with `provider=kimi` — including the two prompt-injection rows (#13–#14) — while Qwen is skipped. The Kimi provider ships with a wiremock suite pinning its exact request shape (no `response_format` json-mode flag) and reuses the same tolerant JSON parser and grounding checks as the primary. Budget guardrails and rate limits apply across the chain, and every consult records `provider_used`, so failovers are auditable in the journal. The first keyed Kimi call remains PENDING-KEY (STUB-02) — model-string confirmation happens then; the failover machinery itself is fully tested.

Evidence: docs/evidence/p08-failover.txt; crates/sentinel/src/brain/chain.rs; crates/sentinel/src/brain/providers.rs

---

## 6. Chainlink — Best workflow with CRE

Our CRE workflow is `sentinel-deadman` — a dead-man's-switch for the Perpl risk guardian. On a cron trigger it GETs the breaker's heartbeat status, cross-checks the latest SentinelAuditAnchor Heartbeat events on-chain, and when a guardian is stale && critical, POSTs the frozen HMAC-signed trigger that fires a protective reduce (dry-run mode). Verified with CRE CLI v1.37.0: `cre workflow build` exit 0 (WASM binary 904b5f689b541712ccf185ffa258a58dfe4e37bf636e51aefa2e8db72fcc49cc), `cre workflow hash` exit 0 (workflow hash 00cb333e5138507c234f329add783079c5f6fc21ce377b568bc4212c05f99cfb). `cre workflow simulate` is PENDING-ACCOUNT (no CRE login / CRE_API_KEY on this host): the documented fallback executed instead — the same `core.ts` definition driven locally, end-to-end exit 0, emulated breaker answering HTTP 202 {"accepted":true,"epoch":1,"fired":true}; the frozen HMAC vector reproduced independently (sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04), Heartbeat topic0 0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee. Login/registration steps are runbooked.

Evidence: docs/evidence/p14-cre-simulate.txt; workflow-cre/sentinel-deadman/main.ts; workflow-cre/sentinel-deadman/src/core.ts; crates/breaker/src/watcher.rs

---

## 7. Envio — Best Use of Envio

Sentinel indexes its own on-chain audit trail with Envio HyperIndex 3.12.1. `indexer/src/EventHandlers.ts` maps SentinelAuditAnchor.DecisionAnchored → `SentinelAnchor` rows and Heartbeat → `SentinelHeartbeat` rows; `docs/queries.graphql` holds the dashboard GraphQL queries. The local anvil end-to-end proof (real deploy, `envio dev`, GraphQL via Hasura) captured a live seq=3 anchor indexed in realtime over RPC — rows seq 1–3 queried back, heartbeat timeline queried, SQL cross-checks matching. A Rust client (`crates/sentinel/src/indexer.rs`) reads anchors/heartbeats/liquidations for the dashboard and the audit-verify cross-check, and degrades gracefully when `ENVIO_GRAPHQL_ENDPOINT` is unset. Perpl exchange events (PerpFill/Liquidation/AccountSnapshotDay) are intentionally empty for now — the exchange ABI is unresolved (STUB-21, roadmap); Envio Cloud deploy and live Monad-testnet rows are PENDING-ACCOUNT/WALLET (STUB-20).

Evidence: docs/evidence/p12-envio-anvil.txt; indexer/src/EventHandlers.ts; docs/queries.graphql; crates/sentinel/src/indexer.rs

---

## PENDING — stated honestly (nothing below is claimed complete)

- Live Perpl testnet reduce / recorded live session — PENDING-KEY (docs/STUBS.md STUB-09, STUB-12); DRY_RUN + offline proofs are green.
- First live Qwen and Kimi keyed runs — PENDING-KEY (STUB-01, STUB-02); offline mock/wiremock proofs are green.
- First paid Nansen x402 call — PENDING-WALLET (STUB-16); the free 402-parse leg is live-green (`--check` rc=0).
- SentinelAuditAnchor testnet deploy + live anchors/heartbeats — PENDING-WALLET (STUB-17); the anvil e2e is green.
- CRE `simulate` / `supported-chains` / registration — PENDING-ACCOUNT (STUB-03); `build` + `hash` are green and the documented local-runner fallback was executed.
- Envio Cloud deploy + live Monad-testnet rows — PENDING-ACCOUNT/WALLET (STUB-20); the local anvil e2e is green.
- Railway deploy + public dashboard URL — PENDING-ACCOUNT (STUB-24); local docker/compose proofs are green.
- Public URLs (dashboard, GraphQL endpoint, demo/pitch videos) — PENDING (STUB-20/24; docs/RUNBOOK.md).
