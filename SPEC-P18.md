# SPEC-P18 — Demo & Pitch Video Scripts + Recording Plan

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 3h scripts (now) + 3h recording (Oct 12, operator). Depends: P06, P13, P14, P15.

## 0. Ownership

Agent Q owns `docs/video/demo-script.md` + `docs/video/pitch-script.md` (+ supporting stills
list). Parent dry-runs `scripts/crash-demo.sh` + `scripts/breaker-demo.sh` and pins the
observed timings/beats into the script notes (agent Q uses only parent-captured or
transcript-captured outputs; never invented screen states).

## 1. demo-script.md (3:00 max; captions burned in; muted-readable)

Timestamped beats with: video (what is on screen), voiceover (sentence per beat, <=20 words),
exact command to run, expected on-screen evidence:
- 0:00-0:20 Hook: dashboard hero (docs/evidence/p15-dashboard-top.png as storyboard still);
  isolated-margin trap in one sentence; position card green 32% distance (uses the real
  dashboard values from the P15 live pass).
- 0:20-0:55 SPEED: `scripts/crash-demo.sh` replay; tier flips green->yellow->orange->red;
  reflex reduces fire; terminal shows the pipeline log lines (quote the real lines from
  docs/evidence/p06-crash-demo.clean.txt); latency overlay: reflex measured 0.271 us,
  order on-chain ~1 s (anvil beats; testnet pending - label).
- 0:55-1:35 JUDGMENT: `/risk` consult storyboard; prompt context (snapshot + Nansen SM
  block); x402: PENDING-WALLET note on screen ("payment rail verified; first paid call
  pending wallet") - do NOT fake a payment; brain_eval scoreboard from
  docs/evidence/p07-brain-eval-mock.txt + failover 14/14 from p08-failover.txt.
- 1:35-2:05 TRUST: dashboard audit verifier; `cargo run --bin audit-verify` output (real:
  "journal consistent; 5 entries; 5 anchored; root matches at seq 5" from
  docs/evidence/p10-anvil-e2e.txt); explorer side-by-side on testnet = PENDING-WALLET:
  storyboard the anvil tx hash 0xe67e2d0d... with the honest overlay.
- 2:05-2:40 RESILIENCE: `scripts/breaker-demo.sh`; kill -9 beat; stale heartbeat; breaker
  fire (HTTP 202 fired:true, journal line, alert text - all real from
  docs/evidence/p14-breaker-demo.txt); label CRE leg (simulate PENDING-ACCOUNT; local-runner
  fallback executed).
- 2:40-3:00 EVIDENCE: backtest panel (frozen aggregate literal); bounty wall; end card
  (repo URL; "every number in this video is in docs/evidence/").

## 2. pitch-script.md (2:00 max)

Problem (personal stake, isolated-margin confusion) -> Why now (Monad speed; agent economy:
x402 + ERC-8004-era trust) -> Solution (five pillars in plain words) -> Market (every perp
trader on Monad; venues next) -> Proof (backtest numbers + local live passes; PENDING items
stated honestly) -> Team -> Ask. Voiceover sentences <=20 words; timing marks every 15 s.

## 3. Recording plan (Oct 12, operator)

OBS scenes pre-built (terminal 14pt+, dashboard fullscreen 1080p60, Telegram desktop,
explorer), record per-beat (retry-friendly), captions burned in, unlisted YouTube, links
into README + submission form. Preconditions checklist: `.env` real keys (Perpl/Qwen/Kimi/
Telegram) -> live beats; if keys still missing on Oct 12: record replay/anvil variant (the
scripts must say exactly what changes: swap live beat for replay + PENDING overlay).
Dry-run requirement: both demos run clean twice before any recording.

## 4. Acceptance (scripts phase)

Both md files complete with exact commands + captured real outputs quoted from evidence
files; storyboard stills list existing PNGs only; parent dry-runs recorded in
docs/video/ with timings. Recordings themselves: Oct 12 (operator) - not part of this phase.

## 5. Standing rules

No invented screens/logs/payments; LC_ALL=C; evidence-shaped report.
