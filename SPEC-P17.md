# SPEC-P17 — README, Architecture Doc & Bounty Evidence Pack

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 4h (final FACTS/live-links pass scheduled Oct 12). Depends: P00-P16 shipped.

## 0. Ownership (disjoint; children never run git)

Agent N (architecture): `docs/architecture.md` ONLY (full rewrite; keep the v0 file's
spirit, expand to the outline below).
Agent O (readme): `README.md` ONLY (root; does not exist yet — create it).
Agent P (bounty answers): `docs/submission/bounty-answers.md` (+ `docs/submission/README.md`
index if useful).
Agent Q (video scripts, SPEC-P18): `docs/video/demo-script.md`, `docs/video/pitch-script.md`.
Agent R (verifier): `scripts/check-links.sh` + `docs/evidence/p17-audit.txt`.

## 1. CANONICAL FACT SHEET (the ONLY allowed source of numbers)

Every number in your artifact MUST come from one of these evidence files (link the file).
Anything else is PENDING/ROADMAP and must be labeled as such (no overclaims — judges verify).

| Fact | Value (exact) | Evidence |
|---|---|---|
| Reflex risk-engine latency | 0.271 us/iteration (10k iters in 2.707756 ms; 2200 intents) | docs/evidence/p04-skill-verification.txt |
| Backtest headline (frozen literal) | "Across 14 scenarios representing $60163.365 notional, Sentinel preserved $5972.13546111693125 (9.93%); baseline liquidations: 11 -> with Sentinel: 0" | docs/backtest-report.md (+ .json) |
| Backtest per-scenario table | see docs/backtest-report.md (12 synthetic + 1 recorded + 1 reconstructed) | docs/backtest-report.md |
| Backtest determinism | byte-identical reruns (minus metrics); 14 scenarios in ~0.03-0.06 s wall on the release binary (re-measured 2026-10-06: 0.063/0.025 s) | docs/evidence/p13-validate.txt (POST-FIX section) |
| Provider failover scoreboard | 14/14 PASS provider=kimi (FORCE_PROVIDER_FAIL=qwen), injection rows 13/14 | docs/evidence/p08-failover.txt |
| Brain eval (mock) | read the exact aggregate/score line from the file and quote it verbatim | docs/evidence/p07-brain-eval-mock.txt |
| Test suite counts (final) | 38 suites / 878 tests OK; clippy -D warnings rc=0; fmt clean | docs/evidence/p16-validate.txt |
| x402 EIP-712 | digest 0xab74a1066d05d2a495f2d5938ba4db686a7039cbf63cd6e456a2f11c641c0951; verified against one committed ethers 6.17 oracle plus an alloy signature-recovery re-check; live --check green (8 rails, Monad rail selected); paid smoke PENDING-WALLET | docs/evidence/p09-x402-check.txt; tests/fixtures/nansen/eip712_oracle.js; crates/sentinel/tests/nansen_adversarial.rs |
| Audit chain (anvil e2e) | 5 entries / 5 anchored / root matches at seq 5; kill -9 crash-safety proven | docs/evidence/p10-anvil-e2e.txt (+ p10 crash-safety files) |
| Anchor contract | contracts/src/SentinelAuditAnchor.sol (events DecisionAnchored/Heartbeat); testnet deploy PENDING-WALLET | SPEC-P10.md; STUBS.md STUB-17 |
| Breaker demo | exit 0 twice; fired HTTP 202 {accepted,epoch,fired:true}; journal reduce 0.635 @ 999.109 notional (dry-run); batch tx 0x390ac28dcf2375221c9eac36da0aff876b7425075f8ef58412f70893c95b4a0d | docs/evidence/p14-breaker-demo.txt |
| Breaker HMAC/topics | sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04 (spec vector, independently reproduced); Heartbeat topic0 0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee | SPEC-P14.md; crates/breaker/src/watcher.rs |
| CRE workflow | cre build exit 0 (binary 904b5f68...), workflow hash 00cb333e...; simulate PENDING-ACCOUNT with executed local-runner fallback | docs/evidence/p14-cre-simulate.txt |
| Envio indexer | local anvil e2e: realtime (RPC) indexing of live seq=3 anchor; cloud PENDING-ACCOUNT | docs/evidence/p12-envio-anvil.txt |
| Dashboard | single-file 61670 bytes, zero external requests (resource-timing proof), live pass with kill-switch round-trip; 5 screenshots docs/evidence/p15-dashboard-*.png | docs/evidence/p15-validate.txt |
| Docker | image digest bdfa6a107a11 (reproducible); compose seq-continuity PASS; non-root; healthcheck | docs/evidence/p16-docker.txt; p16-verify.txt |
| Live testnet / Perpl execution | client+auth verified; DRY_RUN acceptance 2710.98630 (idempotency key 32:reduce:250); live reduce PENDING-KEY | docs/evidence/p05-dryrun-acceptance.txt; STUBS STUB-09/12 |
| Public URLs (dashboard/Railway, Envio Cloud, videos) | PENDING — table cells carry "pending" + the STUB/runbook reference | STUBS.md STUB-20/24; docs/RUNBOOK.md |

## 2. README.md structure (frozen order)

1. Hero: name, one-liner (frozen literal: "The first verifiable, non-custodial AI risk
   guardian for perpetual futures — deterministic reflexes, Qwen/Kimi strategic judgment,
   every action provable on-chain"), badges (tests/clippy/license), dashboard screenshot
   (relative path docs/evidence/p15-dashboard-top.png). Demo GIF: PENDING (P18).
2. The Problem: isolated-margin trap (1-2 paragraphs, no invented stats — cite only facts
   you can source inside the repo or link a public source; if unsourced, phrase qualitatively).
3. The Solution: dual-brain + 5 pillars + ASCII/mermaid diagram.
4. Live links table: dashboard URL (pending), GraphQL endpoint (pending), anchor contract
   explorer (pending testnet), demo/pitch video (pending) — each cell honest.
5. Quickstart: docker compose up + .env guide (link docs/RUNBOOK.md + docs/SETUP-MANUAL.md),
   local replay quickstart that works with ZERO keys (`cargo run --bin sentinel -- --replay
   tests/fixtures/perpl/crash-scenario.jsonl` + dashboard on :8080 + `cargo run --bin backtest`).
6. Bounty sections x7: Perpl API; Perpl Analytics/Risk; Nansen; Qwen; Kimi; Chainlink CRE;
   Envio. EACH: what we built / why it fits the bounty's question / 2-3 code snippets with
   file links / evidence artifacts (>=1 screenshot-or-log + >=1 tx hash-or-log excerpt where
   the row exists in the fact sheet; else PENDING label) / measured numbers from the sheet.
7. Architecture deep-dive: link docs/architecture.md (+ mermaid if used).
8. Backtest results: the frozen aggregate literal + compact per-scenario table.
9. Security notes (honest): key custody (SecretString, .env gitignored), prompt-injection
   eval results, x402 payer isolation, kill switch, rate limits; known limitations list.
10. Roadmap: mainnet path (MPC/session keys, funded deploy, live reduce), multi-venue, Envio
    cloud, CRE registration, ERC-8004-inspired identity, mobile.
11. Team + License (Apache-2.0 per Cargo workspace).

## 3. docs/architecture.md outline

Status header + links; system diagram (mermaid, renders on GitHub); the five pillars;
invariants (reflex never calls an LLM; LLM never bypasses policy; audit-before-action;
external I/O behind mockable traits; core zero-network; never log secrets); dual-brain
rationale with the 0.271 us vs LLM-budget numbers (label live LLM latency unmeasured);
data-flow sequence diagrams (happy path, reflex save, strategy consult w/ x402 purchase,
approval flow, breaker save, audit anchor+verify) as mermaid sequence diagrams;
scalability (stateless reflex sharding by account, venue trait, cost model per guarded
account with CITED list prices + the arithmetic shown, mark estimates as modeled);
security & threat model (key custody tiers, policy envelope, injection results, payer
isolation, kill switch, mainnet plan incl. MPC/session keys + external audit).

## 4. bounty-answers.md

Per bounty: <=200-word answer to the form's specific question, pasted-ready, ending with a
"Evidence:" line of 2-4 exact repo paths. No invented numbers; use the sheet.

## 5. Verification (agent R)

- `scripts/check-links.sh`: bash strict; extract markdown links from README.md +
  docs/architecture.md + docs/submission/*.md; classify (relative file / relative anchor /
  external); relative files must EXIST (fail on missing); external: `curl -I -L --max-time 8`
  best-effort, report status classes, never hang; exit nonzero on missing relative files.
- Honesty audit -> `docs/evidence/p17-audit.txt`: for EVERY numeric claim in the three
  documents, the evidence file it traces to (or a PENDING/ROADMAP label); list of any
  unsourced number (must be zero); mermaid blocks parse-check (no toolchain: heuristic
  fence/pipe balance + note); tables well-formed (pipe scan).
- Acceptance: check-links exits 0 on relative files; zero unsourced numbers; every bounty
  section has >=1 evidence artifact + file links.

## 5bis. Changelog v1.0.1 (integration, 2026-10-06)

- Fact-sheet corrections after the honesty audit (docs/evidence/p17-audit.txt findings):
  backtest runtime re-measured on the release binary (0.975 s figure had no filed evidence
  and is retired; POST-FIX section added to p13-validate.txt); x402 oracle wording corrected
  to one committed ethers oracle + alloy recovery re-check; breaker "exit 0 twice" now points
  at the two filed dry-run transcripts (p18-breaker-dryrun-{1,2}.txt).
- Dashboard size in docs updated to the current 61,807 B (CRE embed +137 B; POST-FIX note in
  p15-validate.txt).
- Audited files re-pinned after these edits (see p17-audit.txt POST-FIX section).

## 6. Standing rules

No git from children; LC_ALL=C; evidence-shaped reports; absolute honesty — every PENDING
stays PENDING until its STUB closes; no screenshots invented (reference only files that exist).
