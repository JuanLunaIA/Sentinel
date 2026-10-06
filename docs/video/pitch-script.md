# Sentinel — pitch video script (2:00 max)

Status: scripts phase (SPEC-P18 §2). Owner: agent Q. Recording: Oct 12, operator.
Voiceover sentences ≤ 20 words; timing marks every 15 s. Numbers follow the SPEC-P17 §1 fact sheet —
sources listed in the appendix. PENDING items are labeled everywhere they are spoken.

| Mark | Section |
|---|---|
| 0:00 | Problem — personal stake + the isolated-margin trap |
| 0:15 | Why now — Monad speed + the agent economy (x402, ERC-8004-era trust) |
| 0:30 | Solution A — pillars 1–2 (Speed, Judgment) |
| 0:45 | Solution B — pillars 3–5 (Trust, Resilience, Evidence) |
| 1:00 | Market |
| 1:15 | Proof — backtest + local passes; PENDING stated honestly |
| 1:30 | Team |
| 1:45 | Ask |

---

## 0:00–0:15 · Problem

**Voiceover** (≤ 20-word sentences):
1. *[Operator — one true sentence of personal stake; say only what actually happened to you.]*
2. "Isolated margin feels safer — each position carries its own collateral." *(10)*
3. "But a gap can take it before you can react." *(10)*

**On screen:** hands-on-keyboard or chart b-roll → a single position card sliding toward its
liquidation line. No invented numbers.

---

## 0:15–0:30 · Why now

**Voiceover:**
1. "Two things changed: Monad made perps fast, and agents got payment rails." *(12)*
2. "x402 for payments; ERC-8004-era trust standards are arriving." *(9)*

**On screen:** Monad logo/block-time motif (qualitative), then "x402" and "ERC-8004-era" text cards.
No invented market stats — this is a thesis segment.

---

## 0:30–0:45 · Solution A — Speed, Judgment

**Voiceover:**
1. "Sentinel is a guardian, not a trading bot — it only de-risks." *(11)*
2. "Pillar one: deterministic reflexes — zero point two seven one microseconds, no LLM in the loop." *(14)*
3. "Pillar two: strategic judgment — Qwen decides, Kimi backs it up." *(10)*

**On screen:** title card "SENTINEL — the verifiable AI risk guardian" → the 4-tier ladder
(Green→Yellow→Orange→Red) → dual-brain diagram (Qwen primary → Kimi failover). Use
`docs/evidence/p06-golden-path.png` if a still is needed.

---

## 0:45–1:00 · Solution B — Trust, Resilience, Evidence

**Voiceover:**
1. "Pillar three: verifiable trust — every decision hash-chained and anchored on Monad." *(11)*
2. "Pillar four: resilience — an independent dead-man's switch if Sentinel goes silent." *(11)*
3. "Pillar five: evidence — reproducible backtests; every number traceable." *(9)*

**On screen:** `✓ CHAIN VERIFIED` panel still (`docs/evidence/p15-dashboard-top.png`) → breaker
timeline card (heartbeat → stale → fire) → backtest bars (`p15-dashboard-audit-resilience.png`).

*(The five pillars in plain words: Speed / Judgment / Trust / Resilience / Evidence — the same five
the demo video walks through, framed per SPEC-P17 §2 pt 3.)*

---

## 1:00–1:15 · Market

**Voiceover:**
1. "Every perpetuals trader on Monad is a potential user — that's the starting market." *(12)*
2. "Built behind a venue abstraction: Perpl first, other venues next." *(10)*

**On screen:** Monad → Perpl mark, then a simple "venue trait" diagram.
Qualitative only — no invented TAM numbers.

---

## 1:15–1:30 · Proof

**Voiceover:**
1. "Backtest: fourteen scenarios, sixty thousand notional, five thousand nine hundred seventy-two preserved — nine point nine three percent." *(16)*
2. "Liquidations: eleven baseline; zero with Sentinel." *(7)*
3. "Local live passes are green; testnet items stay labeled PENDING." *(10)*

**On screen:** the frozen backtest literal
(`docs/backtest-report.md`) + the exact PENDING labels:
`Monad testnet deploy PENDING-WALLET · live LLM keys PENDING-KEY · x402 first paid call PENDING-WALLET · CRE simulate PENDING-ACCOUNT · Telegram live PENDING-TOKEN`.
Never narrate a PENDING item as done.

---

## 1:30–1:45 · Team

**Voiceover:**
1. "Built by Juan Luna — one engineer, one Rust workspace." *(9)*
2. "The whole system — reflex, brains, policy, audit, breaker — ships in one repo." *(13)*
3. "github.com slash JuanLunaIA slash Sentinel." *(5)*

**On screen:** repo URL card (confirm repo is public before publishing) + 4-6 line signs of the
codebase (crates, tests count `38 suites / 878 tests OK` from `docs/evidence/p16-validate.txt`).

---

## 1:45–2:00 · Ask

**Voiceover:**
1. "Our ask is simple: judge us on the evidence, not the adjectives." *(11)*
2. "Fund a testnet wallet, or point us at a real venue — the pending list closes fast." *(16)*
3. "Sentinel. Verify everything." *(3)*

**On screen:** end card — repo URL + **"every number in this video is in docs/evidence/"** + the
PENDING list shown once, unapologetically.

---

## Appendix — spoken numbers and their sources (nothing else may be spoken)

| Spoken | Exact value | Source |
|---|---|---|
| "zero point two seven one microseconds" | 0.271 µs/iteration (10k iters in 2.707756 ms) | `docs/evidence/p04-skill-verification.txt` |
| "fourteen scenarios" / "sixty thousand notional" / "five thousand nine hundred seventy-two preserved" / "nine point nine three percent" / "eleven baseline; zero" | `Across 14 scenarios representing $60163.365 notional, Sentinel preserved $5972.13546111693125 (9.93%); baseline liquidations: 11 -> with Sentinel: 0` | `docs/backtest-report.md` |
| "Qwen decides, Kimi backs it up" | failover 14/14 provider=kimi (FORCE_PROVIDER_FAIL=qwen) | `docs/evidence/p08-failover.txt` |
| "38 suites / 878 tests OK" (on screen only) | 38 suites / 878 tests OK; clippy/fmt clean | `docs/evidence/p16-validate.txt` |
| repo URL | `https://github.com/JuanLunaIA/Sentinel` (Cargo.toml `repository`) | `Cargo.toml` — verify public before publishing |
| team | authors: Juan Luna IA | `Cargo.toml` `authors` field |

PENDING inventory (STUBS.md): STUB-01/02 (live LLM keys) · STUB-09/12 (Perpl testnet live) ·
STUB-16 (x402 first purchase) · STUB-17 (testnet deploy) · STUB-18 (Telegram token) ·
STUB-03 (CRE account) · STUB-20 (Envio cloud) · STUB-24 (Railway deploy).

Honesty guards: the personal-stake line is operator-supplied and must be true; no payment is ever staged;
every number spoken above is quoted from the files listed; if a still is used it must be one of the
existing PNGs in `docs/evidence/` (see the demo script's stills inventory).
