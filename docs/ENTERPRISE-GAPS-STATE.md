# Enterprise evaluation — implementation state

Source: `SENTINEL-SUPREME-ENTERPRISE-EVALUATION.md` (74 gaps, Part IX).
State as of 2026-10-13.

## Shipped

### P22 — GAP-009 volatility-adaptive bands + GAP-028 liquidation horizon
Commit on `main`, CI green, 40 suites / 923 tests.

`crates/sentinel-core/src/vol.rs` (pure, 380 LOC) + 11 adversarial/property
tests + `--adaptive` on the backtest binary + threshold provenance in every
action record.

Measured on both frozen suites:

| suite | capital preserved | liquidations avoided | false positives |
|---|---|---|---|
| curated (14) | $5,972.14 → **$6,410.42** (+7.3%) | 11 → 11 (preserved) | 3 → 3 |
| realistic (12) | $394.02 → **$650.16** (+65.0%) | 1 → 1 (preserved) | 20 → 29 |

The realistic-suite FP rise is a **reported falsification**, not a tuned
number: the harness counts every reduce as a false positive whenever the
baseline never liquidated, so protection and "FP" are the same event on that
suite (same 8 scenarios in both columns, correlation +0.561). Full write-up in
`docs/evidence/p22-adaptive-bands.txt`; plan and attack notes in
`docs/PLAN-adaptive-bands.md`.


### P23 — GAP-014 layer 1: proof-carrying AI decisions (inference commitment)
Commits on `main` (plus a clippy fix), CI green, **42 suites / 932 tests, 0 failures**.

`sentinel-core/src/inference.rs` + `bin/inference-verify` + archive sidecar +
9 adversarial tests. Every STRATEGY journal entry now carries
`inference_commitment`: SHA-256 over a length-prefixed encoding of the system
prompt, schema, user prompt, model id, provider, sampling parameters and the
raw completion. It rides the decision document, so it is covered by
`entry_hash`, the Merkle batch and the on-chain anchor.

Length framing is load-bearing: plain concatenation lets `("ab","c")` and
`("a","bc")` collide, which would let text move between prompts without
changing the commitment. Asserted by test and demonstrated against the
verifier.

End-to-end evidence: the encoding was re-implemented independently in Python
and both implementations agree byte for byte (the same cross-language oracle
discipline as the Ed25519, EIP-712 and journal SHA-256 checks). Verifier paths
measured: honest `rc=0`; rewritten archive `rc=1`; field-boundary shift
`rc=1` with distinct digests; orphan record `rc=1`. Full write-up in
`docs/evidence/p23-inference-commitment.txt`.

Not built (GAP-014 layer 2): TEE remote attestation, which is what would
prove the execution environment itself. The evidence file states that boundary
explicitly.


### After the P22–P28 series (tests 39 -> 44 suites, 912 -> 955)

| item | state | evidence |
|---|---|---|
| GAP-009/028 volatility-adaptive bands | **shipped** (`--adaptive`) | `p22-adaptive-bands.txt` |
| GAP-014 layer 1, inference commitment + `inference-verify` | **shipped** | `p23-inference-commitment.txt` |
| GAP-029 per-market budgets, GAP-024 policy fingerprint | **shipped** (fingerprint adopted in every strategy intent) | `p24-policy-budgets.txt` |
| GAP-007 portfolio: correlation, equity-at-risk, planner | **built and tested**, planner measured inert | `p25-portfolio-layer.txt`, `p26-cascade-wiring.txt` |
| GAP-007 reserve + segregation enforcement | **built and wired, measured harmful** (-624%); off by default | `p27-allocator.txt`, `p28-contention-family.txt` |
| GAP-007 allocator commit | **not built** — the next step | `p28-contention-family.txt` |
| contention scenario family (new instrument) | **shipped**, deterministic | `p28-contention-family.txt` |
| P16 supervisor flake | hardened, no assertion weakened | `p16-flake-hardening.txt` |

Three of these are negative results published with their mechanism. On a
"don't trust, verify" product that is the point: the measurement is the
deliverable, and every number above can be reproduced from the repository.

## The 74 gaps, sorted by what is actually actionable

### A. Pure code, no external dependency — can be built now
Highest value first, per the evaluation's own ranking:

| Gap | What | Effort | Notes |
|---|---|---|---|
| GAP-006 | Depth-aware execution: book-implied sizing, TWAP/iceberg, reprice-retry, partial-fill re-plan | L | needs the order-book feed wired (`/v1/market-data/:id/book`) |
| GAP-007 | Portfolio-level risk aggregation, correlated-cascade ordering | L | pure core + pipeline |
| GAP-008 | Funding-rate & ADL modeling: collateral drift in the liq math, account-history events 9/10 | M | pure-core change, SDK-vector-testable |
| GAP-010 | Feed integrity: multi-source mark plausibility, book-abnormality, RPC quorum | M | needs a second price source |
| GAP-011 | Observability: Prometheus/OTel metrics, SLO catalog, alert routing | M | telemetry.rs fan-out already exists |
| GAP-012 | HA: journal fencing (writer_id + epoch), active-passive, game-days | L | `AuditJournal` trait is the seam |
| GAP-013 | AI model council + confidence calibration (isotonic/temperature) | L | needs the live corpus for labels |
| GAP-014 | ~~Verifiable inference, layer 1~~ **DONE (P23)**. Layer 2 (TEE attestation) and layer 3 (zk proof of the deterministic envelope) remain | M→L | layers 2–3 need confidential-computing infra and proving tooling |
| GAP-015 | Backtester v2: walk-forward, Monte Carlo, funding, depth, liquidation fees | L | builds on P13 + P21 |
| GAP-016 | Prompt-injection red team at scale (2 fixtures → 100+) | M | corpus generation |
| GAP-021 | Redundant alert channels + approval-latency engineering | S | notify.rs sinks |
| GAP-022 | Supply chain: cargo-audit/deny in CI, SBOM, cosign, fuzzing, coverage gates | M | mostly CI + 1 day |
| GAP-024 | Governance: policy-version journaling, dashboard mutations chained, dual-secret rotation | S | cheap, institutional-grade |
| GAP-025 | Event-driven post-fill verification (mt:25/mt:27 streams) | S | |
| GAP-026 | Breaker hardening: market-shock corroboration, mutual watchdog, triple-sourced fire | M | |
| GAP-027 | Venue-parameter monitoring + min-size/tick calibration loop | S | |
| GAP-029 | Per-market policy budgets & velocity limits | S | |
| GAP-030 | Public proof pages ("Proof of Save") | M | dashboard + Envio |
| GAP-032 | Docs site, ADR log, cargo doc publishing, changelog discipline | S | |
| GAP-033 | Tool-use brain (function-calling into deterministic data) | M | |
| GAP-035 | Shadow mode & counterfactual learning loop | M | |
| GAP-037 | Brain cost/latency optimization | S | |
| GAP-041 | Enterprise identity: RBAC, SSO/OIDC, M-of-N approvals | M | |
| GAP-049 | What-if simulation console | M | |
| GAP-050 | Collateral (AUSD) health pack | S | |
| GAP-057 | Public status page | S | |
| GAP-060 | Grafana plugin pack | S | |
| GAP-062 | Onboarding wizard / guided testnet tour | S | |
| GAP-070 | WCAG 2.2 AA across surfaces | S | |
| GAP-074 | Public RFC process | S | docs |

### B. Needs credentials/accounts the operator must supply
GAP-003 (funded mainnet sequence), GAP-004 (live-key LLM eval + drift monitor),
GAP-017 (multi-tenant SaaS), GAP-019 (mobile), GAP-036 (self-hosted sovereign
brain), GAP-042 (ERC-8004 registration), GAP-046 (risk-API x402 merchant),
GAP-053/054 (billing, pricing).

### C. Needs third parties, money, or legal process — not code
GAP-001 (external audit — the #1 ranked gap overall), GAP-002 (MPC custody
vendor), GAP-005 (multi-venue program, Hyperliquid first: XL), GAP-018
(insurance/SLA), GAP-020 (SOC 2 path), GAP-039 (MRM framework), GAP-043/044/045
(cascade network, marketplace, white-label), GAP-055/056 (legal, support),
GAP-063..069, GAP-071..073 (partnerships, community, token study, branding,
analyst relations, academic).

**Honest read:** roughly 30 of the 74 are code that a competent engineer can
ship without leaving the repo; the rest are capital, partnerships, or process.
The evaluation's own top-3 (audit, MPC, funded mainnet) are all in category C,
which means the *ranked* list and the *implementable* list barely overlap at
the very top. The highest-ranked items that are reachable from the keyboard are
GAP-014, GAP-006, GAP-007, GAP-010 and GAP-024.

## Recommended next slice

**GAP-014 layer 1 (commitment layer)** — the evaluation calls it "the keystone
of the don't-trust, verify positioning" and rates it M effort. It is a pure
addition to the strategy path: hash
`system_prompt ‖ SCHEMA_DOC ‖ user_prompt ‖ model_id ‖ provider ‖ temperature ‖ max_tokens ‖ raw_completion`
into every STRATEGY journal entry, ride the existing Merkle batches, and ship a
reference verifier binary that replays a decision from committed inputs and
asserts the journaled outcome. Nothing existing changes shape.
