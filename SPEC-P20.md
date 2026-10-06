# SPEC-P20 — Autonomous Strategy Consults (closes STUB-15) + Judge Demo

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Depends: P07/P08 engine, P09 nansen client, P10 journal, P11 bot, P06 pipeline.
Goal: with real keys, the deployed instance demonstrates AUTONOMOUS judgment — the
strategy brain consults on its own triggers, through the policy gate, journaled as
STRATEGY, executed when allowed — not only when a human types /risk.

## 0. Hard guardrails (additive; every existing suite stays byte-identical)

- The consult machinery is ACTIVE only when a strategy engine exists (ENABLE_STRATEGY +
  provider key). Engine absent (replay tests, no keys) => zero new behavior; the
  pipeline keeps emitting the existing `consult_scheduled` alert for Yellow entries.
- `cargo test --workspace` must stay green INCLUDING pipeline_determinism (byte-identical
  replay), p06/p10/p11 suites. Do not touch their fixtures.

## 1. Ownership (disjoint; children never run git)

Agent S1 (consult-core): `crates/sentinel/src/consult.rs` (NEW, full implementation),
`crates/sentinel/src/pipeline.rs`, `crates/sentinel/src/main.rs`,
`crates/sentinel/src/bot/mod.rs` + `crates/sentinel/src/bot/handlers.rs`
(engine becomes `Option<Arc<StrategyEngine<QwenProvider, KimiProvider>>>`),
`crates/sentinel/tests/p11_adversarial.rs` (MECHANICAL constructor updates only —
type ripple; note it in the report; the p11 suite must stay green).
Agent S2 (judge-demo + alert classes): `scripts/judge-demo.sh` (NEW);
`crates/sentinel/src/anchor.rs` + `crates/sentinel/src/notify.rs`
(alert classes per §4 only).
Agent S3 (verifier): `crates/sentinel/tests/p20_adversarial.rs` + fixtures.
Parent: SPEC + stub + integration + STUBS/README/arch updates.

## 2. Behavior (frozen)

Triggers (per SPEC-P07 §5): (a) Yellow-ENTRY transition per market; (b) post-reflex
review after an executed Orange/Red reduce; (c) periodic portfolio review every
`STRATEGY_REVIEW_INTERVAL_SECS` (new env, default 1800) while any position >= Yellow.
All triggers funnel into one consult task; the ENGINE enforces per-market rate limit
(STRATEGY_MIN_INTERVAL_SECS) and the hourly/daily budget (budget breach => its existing
ESCALATE synthetic outcome + alert).

Consult task (owns an `Arc<StrategyEngine>`, a `LiveState` handle, cfg, journal, an
`mpsc::Sender<ApprovedOrder>` toward the pipeline, an AlertSink):
1. Build `ConsultInput` from the live snapshot (account, markets, focus_market,
   policy_summary, reflex summary from recent journal/actions).
2. SM context: if `cfg.nansen` has a payer key, fetch cache-first (existing
   `nansen::cache` TTL) for the focus market's base asset; record spend via the
   existing ledger; ANY error/timeout (cap 5 s) => `SmartMoneyContext::unavailable`
   + warn. Without key => unavailable (current behavior).
3. `engine.consult(&input, now_ms)` -> ConsultOutcome (journal STRATEGY intent before
   the call per audit-before-action, outcome after; entry `trigger: STRATEGY`).
4. Convert the Decision to Intent semantics identical to the bot path (REDUCE/CLOSE/
   ADD_COLLATERAL/HOLD/ESCALATE mapping already used for /risk) and run
   `PolicyEngine::evaluate` (source Strategy).
   - Allow + sized order => send `ApprovedOrder{order, decision_id}` to the pipeline;
     the pipeline submits through its existing executor path (same submit + event +
     alert code as reflex), journals the STRATEGY outcome with execution status.
   - NeedsApproval => journal + alert only ("strategy decision needs approval — use
     /approve or /risk"): no keyboard from the daemon path (documented).
   - Deny/HOLD/ESCALATE => journal + alert (ESCALATE already alerts via engine).
5. Alerts on every consulted decision in Orange+ (reuse notify kinds where possible;
   new `StrategyDecision` kind allowed if done additively in notify.rs — S2 must not
   collide: S1 requests it, S2 adds it).

main.rs: build the engine ONCE as `Arc<...>` when ENABLE_STRATEGY and a key exist
(bot uses the same Arc; consult task spawned via `supervisor::spawn("consult", ...)`
in all run modes; channel receiver handed to the pipeline builder (`.with_strategy_rx`).

## 3. Determinism notes

Pipeline gains an `Option<mpsc::Receiver<ApprovedOrder>>`; None (tests/replay) = exact
old path. Consult task not spawned when engine None. p11/p06/p10 suites: unchanged
behavior; p11 constructors updated mechanically for the Arc type.

## 4. S2 — judge demo + alert classes

`scripts/judge-demo.sh` (unattended, zero-key): builds once, starts the daemon in
REPLAY (PORT configurable, default 8090) with dashboard, waits /healthz, prints a
tour block: what to open (http://localhost:8090/), which panels to look at in order
(positions -> decision feed -> audit verifier -> backtest), runs `backtest --scenario
all` and `audit-verify` once for fresh output, optional `--docker` flag that shells
to compose; trap cleanup; exit 0; transcript hints for the video plan.
Alert classes (STUB-19 partial): anchor task emits a `notify` Alert + logs when
heartbeat/batch anchoring fails N=3 consecutive times, and when it recovers; budget
80% warn stays in the engine guard (already logged) — do NOT duplicate it.

## 5. Acceptance / evidence

- New `crates/sentinel/tests/p20_adversarial.rs` (S3): mock-provider engine drives the
  consult task black-box: Yellow-entry fires exactly one consult (rate limit suppresses
  repeats); budget breach => ESCALATE synthetic + alert, no execute message; Allow =>
  ApprovedOrder received by a fake pipeline receiver; NeedsApproval => alert + no
  message; journal STRATEGY entries chain-verifiable (verify_chain); SM unavailable
  path; engine-None => zero consults; regression: p11 + pipeline_determinism green.
- judge-demo.sh runs clean twice (transcript `docs/evidence/p20-judge-demo.txt`).
- After integration: STUB-15 -> Resolved; README/architecture STUB-15 mentions updated
  by parent (pins refreshed); full gates green.

## 5bis. Changelog v1.0.1 (integration, 2026-10-06)

- (a) Rate-limit clock: the consult task paces the per-market window on the wall clock
  (the engine's own limiter), not the live-state logical clock — spec-silent; replay-mode
  consult pacing is therefore not logical-clock-derived. Accepted; the enforceable
  invariant (one decision outcome per market per window) is pinned by the verifier.
- (b) `notify::AlertSink::send` signature hardened to
  `fn -> impl Future<Output=Result<()>> + Send` (required to supervise the consult task;
  every existing async-fn impl satisfies it unchanged) — additive.
- (c) `AlertKind::StrategyDecision` + `AlertKind::AnchoringDegraded` added additively
  (kind labels `strategy_decision` / `anchoring_degraded`).
- (d) Anchoring alerts are delivered to whatever sink the anchor task receives; the daemon
  entry hands it a TracingSink (Telegram routing for these classes = STUB-19 partial).
- (e) judge-demo pacing: `SENTINEL_MOCK_CAP_MS=800` default inside the script so the full
  tier cascade fits the hold window (probe table in the S2 report); env overrides kept.
- (f) Pipeline gained two mechanical `alert_kind_text` arms for the new kinds (exhaustive
  match); no behavior change elsewhere.

## 6. Standing rules

No git from children; latest versions via live registries; loud stubs; no network in
tests (mock providers + wiremock); clippy -D warnings + fmt gates; evidence-shaped
reports {files_created, tests[{cmd,exit,observed}], open_issues}; LC_ALL=C.
