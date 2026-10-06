# Sentinel — demo video script (3:00 max)

Status: scripts phase (SPEC-P18 §1). Owner: agent Q. Frozen beats: SPEC-P18 §1. Recording: Oct 12, operator.
Captions burned in; the video is muted-readable (every key value also appears as on-screen text).
Voiceover sentences are ≤ 20 words. Any screen that could not be captured is labeled **storyboard**
and names the existing still that stands in for it. Every quoted line/number below traces to a file in
`docs/evidence/` (cited inline) or to the dry-run logs recorded in § "Recording plan + dry-run results".

Labels used, and always shown on screen when the thing is not yet done:
`PENDING-WALLET` (funded wallet), `PENDING-KEY` (live LLM keys), `PENDING-ACCOUNT` (CRE/Envio/Railway accounts),
`PENDING-TOKEN` (Telegram token). **Never fake a payment, a screen state, or a number.**

## Beat map

| Time | Beat | Screen | Storyboard still (existing files only) |
|---|---|---|---|
| 0:00–0:20 | Hook — the isolated-margin trap | Dashboard hero (live session) | `docs/evidence/p15-dashboard-top.png` |
| 0:20–0:55 | SPEED — reflex de-risking | Terminal running the replay | `docs/evidence/p06-golden-path.png` |
| 0:55–1:35 | JUDGMENT — consult, x402, scoreboards | Telegram consult + terminal + dashboard panel | `docs/evidence/p15-dashboard-positions-feed.png` (stand-in; no Telegram capture exists) |
| 1:35–2:05 | TRUST — audit verifier + explorer | Dashboard verifier + terminal + explorer side-by-side | `docs/evidence/p15-dashboard-top.png` (verifier panel visible) |
| 2:05–2:40 | RESILIENCE — dead-man's-switch | Terminal running the breaker demo | `docs/evidence/p14-cre-simulate.png` (CRE leg only) |
| 2:40–3:00 | EVIDENCE — backtest + close | Dashboard backtest panel + cards | `docs/evidence/p15-dashboard-audit-resilience.png` |

---

## Beat 1 — 0:00–0:20 · Hook (dashboard hero)

**Video.** Full-screen dashboard (`http://127.0.0.1:8091/`, 1080p60), slow push-in on the POSITIONS
panel; the ETH card is GREEN. Cut to the tier bar as the VO ends.

**Voiceover** (≤ 20-word sentences):
1. "This is Sentinel — the verifiable AI risk guardian for perpetual futures." *(11)*
2. "On isolated margin, a gapping crash can liquidate you before you can react." *(13)*
3. "ETH market thirty-two sits green — twenty-eight point four percent from liquidation." *(11)*
4. "When tiers slip, Sentinel de-risks instantly — and proves every action on-chain." *(12)*

**Exact command** (three terminals; configuration per the P15 live pass):
```bash
# T1 — local chain
anvil --silent --port 8548

# T2 — deploy the anchor contract (anvil account #0 throwaway key, as in scripts/breaker-demo.sh)
cd contracts && forge create src/SentinelAuditAnchor.sol:SentinelAuditAnchor \
  --rpc-url http://127.0.0.1:8548 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --broadcast

# T3 — daemon + dashboard (PORT 8091, ENABLE_ANCHOR=true, HEARTBEAT_INTERVAL_SECS=8, SENTINEL_MOCK_PACE=1)
ENABLE_ANCHOR=true ANCHOR_CONTRACT_ADDRESS=0x5FbDB2315678afecb367f032d93F642f64180aa3 \
RPC_SIGNER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
HEARTBEAT_INTERVAL_SECS=8 PERPL_RPC_URL=http://127.0.0.1:8548 \
EXECUTION_MODE=DRY_RUN PORT=8091 SENTINEL_MOCK_PACE=1 \
cargo run --bin sentinel -- --mode dry-run --replay tests/fixtures/perpl/crash-scenario.jsonl

# browser
open http://127.0.0.1:8091/
```

**Expected on-screen evidence** (transcribed from the `p15-dashboard-top.png` still):
- Header: `SENTINEL — the verifiable AI risk guardian · DRY_RUN · feed 9h 22m ago · beat 2s ago tx 0xac4cd66d41… · up 1m 06s · v0.1.0`
- `ETH #32 LONG 10 · 2,700.00 → 2,065.95 · 28.4% to liq 1,479.00 · GREEN · collateral $13,560.00`
- `BTC #16 LONG 0.1 · 95,000.00 → 95,000.00 · 35.0% to liq 61,750.00 · GREEN · collateral $3,800.00`
- `EQUITY $-1,340.50 · FREE $5,000.00`

**Storyboard still:** `docs/evidence/p15-dashboard-top.png`.
*Note (open issue): SPEC-P18 §1 mentions a "32% distance" hook card; the captured P15 pass shows
ETH#32 at 28.4% and BTC#16 at 35.0% — this script uses the captured values (never the unverifiable 32%).*

---

## Beat 2 — 0:20–0:55 · SPEED (`scripts/crash-demo.sh` replay)

**Video.** Terminal full-screen (14pt+), the replay log printing in real time. In the edit, cut between
the tier flips. If you time-compress the log, burn in "replay · time-compressed" — the run itself is honest replay.
Optional cut-ins: the dashboard position card flipping 🟢→🟡→🟠→🔴 (B-roll of the same live session).

**Voiceover** (≤ 20-word sentences):
1. "Replay the crash: ETH slides from thirty percent to six percent away from liquidation." *(14)*
2. "Yellow books a consult; orange and red fire deterministic reduces." *(10)*
3. "No LLM sits on the reflex path — it measures zero point two seven one microseconds." *(15)*
4. "Each reduce is journaled, simulated, and anchored on-chain in about a second here." *(14)*
5. "Monad testnet execution is next — pending a funded wallet." *(9)*

**Exact command:**
```bash
export PORT=8091   # avoids the vaultwarden clash on :8080 — see Recording plan
bash scripts/crash-demo.sh
```

**Expected on-screen evidence** — exact lines from `docs/evidence/p06-crash-demo.clean.txt`
(reproduced in both dry runs; timestamps vary per run, message text does not):
```
2026-10-06T01:05:27.951058Z  INFO sentinel::alert: alert kind="tier_change" market_id=32 text=🟡 ETH#32 GREEN→YELLOW · distance 24.10% · consult scheduled
2026-10-06T01:05:27.951286Z  INFO sentinel::alert: alert kind="consult_scheduled" market_id=32 text=🧠 ETH#32 YELLOW · consult scheduled · distance 24.10%
2026-10-06T01:05:57.019886Z  INFO sentinel::alert: alert kind="tier_change" market_id=32 text=🟠 ETH#32 YELLOW→ORANGE · distance 14.88%
2026-10-06T01:05:57.020251Z  INFO sentinel::alert: alert kind="reflex_action" market_id=32 text=⚡ ETH#32 reduce 25% · size 2.500 · sentinel-32-1 · simulated
2026-10-06T01:06:15.976321Z  INFO sentinel::alert: alert kind="tier_change" market_id=32 text=🔴 ETH#32 ORANGE→RED · distance 7.38%
2026-10-06T01:06:19.177908Z  INFO sentinel::alert: alert kind="reflex_action" market_id=32 text=⚡ ETH#32 reduce 50% · size 5.000 · sentinel-32-2 · simulated
2026-10-06T01:06:19.181100Z  INFO sentinel: pipeline finished events=128 executed=2 alerts=6 denied=0
```

**Latency overlay** (burned-in caption, appears over the terminal at the end of the beat):
`reflex: 0.271 µs/iteration (measured, 10k iters) · on-chain anchor: ~1 s on local anvil (measured 0.55 s) · Monad testnet: PENDING-WALLET`
Sources: `docs/evidence/p04-skill-verification.txt` — `classification: 10000 iterations in 2.707756ms (0.271 µs/iteration), intents=2200`;
`docs/evidence/p14-breaker-demo.txt` — `anchor: service started next_seq=0` at `…05:57:11.505247Z` → `anchor: batch anchored from_seq=1 entries=4 tx=0x390ac28d…` at `…05:57:12.051506Z` (0.55 s);
`docs/evidence/p10-anvil-e2e.txt` — the anvil e2e test `finished in 1.00s`.

**Storyboard still:** `docs/evidence/p06-golden-path.png` (a real render of this exact log).
Command note: `scripts/crash-demo.sh` rewrites `docs/evidence/p06-crash-demo.txt` on every run — expected, not a bug.

---

## Beat 3 — 0:55–1:35 · JUDGMENT (`/risk` consult, x402, brain scoreboard)

**Video.** Three quick scenes: (a) phone/TG-style shot — the `/risk` consult card; (b) terminal — the
offline brain scoreboards; (c) dashboard panel — the Nansen x402 spend state.

**Voiceover** (≤ 20-word sentences):
1. "On Yellow, Sentinel consults its strategy brain instead of guessing." *(10)*
2. "The prompt carries the position snapshot and a Nansen smart-money block." *(11)*
3. "Qwen decides in strict JSON; if Qwen fails, Kimi answers instead." *(11)*
4. "Failover scoreboard: fourteen of fourteen — every row provider Kimi." *(10)*
5. "Brain eval: twelve of twelve core action accuracy, fully grounded." *(10)*
6. "The x402 rail is verified; the first paid call is pending a funded wallet." *(13)*
7. "Pending means pending — nothing here is faked." *(8)*

**Exact commands** (live leg first; offline reproductions below — all work with zero keys):
```bash
# (a) live consult — needs .env keys + Telegram token (else: storyboard/offline variant below)
cargo run --bin sentinel -- --mode dry-run        # bot polling; then send  /risk  in Telegram

# (b) offline scoreboards (reproduce the evidence files)
cargo run --bin brain_eval -- --mock
FORCE_PROVIDER_FAIL=qwen cargo run --bin brain_eval -- --mock
```

**Expected on-screen evidence:**
- (a) `/risk` reply: action, confidence, urgency, reason, provider, policy verdict — or an honest
  `DEGRADED` line when keys are placeholders (`docs/telegram-manual-test.md` row 3).
  **Live leg is PENDING-TOKEN (STUB-18) — no Telegram capture exists; use the storyboard variant below.**
- (b) from `docs/evidence/p07-brain-eval-mock.txt`:
  `action-class accuracy (core): 12/12` · `schema validity: 14/14` · `injection schema validity: 2/2` · `grounding: 14/14`;
  sample row: `PASS 01-deep-underwater-red action=REDUCE expected=REDUCE|CLOSE schema=ok grounding=ok (0ms)`.
  Failover, from `docs/evidence/p08-failover.txt`: `…PASS 14-injection-note action=ESCALATE … provider=kimi (0ms)`
  — `action-class accuracy (core): 12/12` with **every** row carrying `provider=kimi`.
- (c) dashboard Nansen panel (see still): `0 calls · $0.00 total spend` / `no purchases recorded`;
  overlay (verbatim per SPEC-P18): **"payment rail verified; first paid call pending wallet"**.
  Free-leg proof, `docs/evidence/p09-x402-check.txt`: `selected Monad rail (eip155:143): asset=0x754704Bc059F8C67012fEd69BC8A327a5aafb603 amount=10000 payTo=0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f maxTimeoutSeconds=300 domain=USDC/2` + `check rc=0`. **Do not stage a purchase.**

**Storyboard still:** `docs/evidence/p15-dashboard-positions-feed.png` — stands in for the Telegram consult
(shows the decision feed + Nansen x402 panel + resilience strip). The `/risk` screen itself is **storyboard**
(no capture exists; PENDING-TOKEN). Offline-variant overlay if keys are missing: `MOCK chain — live keys PENDING-KEY (STUB-01/02)`.

---

## Beat 4 — 1:35–2:05 · TRUST (`audit-verify` + explorer side-by-side)

**Video.** Dashboard AUDIT VERIFIER panel ("✓ CHAIN VERIFIED"), then full-screen terminal with the
`audit-verify` output, then a split screen: that output beside the explorer row for the transaction.

**Voiceover** (≤ 20-word sentences):
1. "Every intent is hashed into a journal before execution runs." *(10)*
2. "Audit-verify walks the chain: consistent, five entries, five anchored, root matches at sequence five." *(14)*
3. "Beside it, the same flow's transaction on the explorer — labeled local anvil." *(12)*
4. "The Monad testnet deployment stays pending until the wallet is funded." *(11)*

**Exact commands:**
```bash
cargo run --bin audit-verify        # against the session's journal (dashboard: GET /api/audit/verify)
bash scripts/p10-anvil-e2e.sh       # reproduces the exact quoted line below, locally
```

**Expected on-screen evidence** — exact line from `docs/evidence/p10-anvil-e2e.txt`:
```
p10 anvil e2e ok: AnchorRunReport { batches: 1, entries_anchored: 5, heartbeats: 1, failures: 0 }; 1 DecisionAnchored event(s); audit-verify: ✅ journal consistent; 5 entries; 5 anchored; root matches at seq 5 |   https://testnet.monadexplorer.com/tx/0xe67e2d0dbc13c0909b60c053985cbd4e9693ee2a8006d0da7f2c8fb6692875ed
```
- Dashboard verifier panel (from the still): `✓ CHAIN VERIFIED · VERIFIED ENTRIES 39 · VALID UP TO SEQ #38 · FIRST SEQ #0 · ANCHORED SEQ #39 · RUNNING ROOT 220c9aa59471… · LATEST ANCHOR TX tx 0xac4cd66d41…`
  (live counts vary per session — keep the run's real numbers; `/api/audit/verify` returns `{entries, first_seq, valid_up_to_seq, broken_at}` per `docs/evidence/p15-validate.txt`).
- Explorer side-by-side: show the transaction row for the anvil tx `0xe67e2d0d…` **with the honest overlay**:
  **"local anvil run (chain 31337) — Monad testnet deploy PENDING-WALLET (STUB-17)"**.
  *The explorer link is the format audit-verify prints (`EXPLORER_TX_BASE` in `crates/sentinel/src/bin/audit_verify.rs`);
  the tx itself ran on local anvil. Never present the anvil hash as a live testnet transaction.*
- **Never claim** the explorer shows a landed testnet tx while STUB-17 is open. With a funded `RPC_SIGNER_KEY`, swap to the real testnet tx (same beats).

**Storyboard still:** `docs/evidence/p15-dashboard-top.png` (verifier panel). Explorer side-by-side has
**no still — render an overlay card** (never a mocked explorer page).

---

## Beat 5 — 2:05–2:40 · RESILIENCE (`scripts/breaker-demo.sh`)

**Video.** Terminal full-screen: the demo banner, then hard cut to the `kill -9` moment, the stale status,
and the fire. Keep the kill and the fire at full speed; trim only the waiting sections (they are real waits).

**Voiceover** (≤ 20-word sentences):
1. "What if Sentinel itself goes down? Kill minus nine the daemon." *(11)*
2. "Heartbeats stop; after twenty-four seconds the guardian is stale." *(10)*
3. "An independent breaker fires: HTTP two-oh-two, fired true." *(10)*
4. "It journals a half-size reduce — simulated, with a dry-run fill." *(11)*
5. "The Chainlink CRE workflow compiles; simulation awaits an account, so the local runner executes it." *(15)*
6. "One dead process never means an unguarded position." *(9)*

**Exact command:**
```bash
bash scripts/breaker-demo.sh
# CRE leg (PENDING-ACCOUNT fallback, executed):
cd workflow-cre/sentinel-deadman && BREAKER_ARM_SECRET=demo-secret node tools/local-runner.ts --config config.local.json --expect fire --now-ms 1791240000000
```

**Expected on-screen evidence** — exact lines from `docs/evidence/p14-breaker-demo.txt`
(PIDs/epoch counters vary per run; the outcomes below are stable across runs — verified twice on 2026-10-06):
- kill: `scripts/breaker-demo.sh: line 246: 12615 Killed   …` then `daemon dead; no more heartbeats will be posted`
- stale: `{"…","age_secs": 26,"max_tier": 3,"stale": true,"critical": true,"armed": true}`
- fire: `POST /breaker/trigger -> HTTP 202 {"accepted":true,"epoch":3,"fired":true}`
- journal line (first fire, condensed — full line in the file):
  `{"ts_ms": 1791266273905, … "epoch": 3, "mode": "dry_run", "market_id": 32, "fraction": 0.5, "size": 0.635, "notional_usd": 999.109, "status": "simulated", "detail": "… client_order_id sentinel-32-4; sentinel-unresponsive-heartbeat-stale-critical; …"}`
- alert (excerpt): `🚨 BREAKER: Sentinel unresponsive: guardian 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266 stale 26s — reduce 0.5 × 10 = 0.635 @ mark 1573.4 (notional 999.1090, distance 5.99% [fallback market metadata: size_decimals=3, min_size=0]); dry-run fill at 1565.5330 …`
- anchor batch tx: `tx=0x390ac28dcf2375221c9eac36da0aff876b7425075f8ef58412f70893c95b4a0d` (from the daemon log tail)
- close: `DEMO COMPLETE — dead-man's-switch fired with exit 0`

**CRE leg label on screen:** `CRE simulate: PENDING-ACCOUNT (STUB-03) — local-runner fallback executed`.
Fallback proof from `docs/evidence/p14-cre-simulate.txt`: `[runner] [step 3/4] breaker answered HTTP 202: {"accepted":true,"epoch":1,"fired":true}`
(`cre workflow simulate` itself exits 1: `✗ Authentication required: not logged in and no CRE_API_KEY set`).

**Storyboard still:** `docs/evidence/p14-cre-simulate.png` (CRE-leg card; the terminal beats are shot live from the script — no still exists).
Command note: `scripts/breaker-demo.sh` rewrites `docs/evidence/p14-breaker-demo.txt` on every run — see Recording plan.

---

## Beat 6 — 2:40–3:00 · EVIDENCE (backtest + bounty wall + end card)

**Video.** Dashboard backtest panel with the 14 bars, then the bounty wall card, then the end card.

**Voiceover** (≤ 20-word sentences):
1. "Fourteen scenarios, sixty thousand dollars notional, five thousand nine hundred seventy-two saved — nine point nine three percent." *(17)*
2. "Baseline liquidations: eleven. With Sentinel: zero." *(7)*
3. "Every number in this video is in docs slash evidence." *(9)*

**Exact command:**
```bash
cargo run --bin backtest        # dashboard panel: GET /api/backtest (same data)
```

**Expected on-screen evidence** — frozen aggregate literal (`docs/backtest-report.md`, also rendered on the
dashboard panel in `p15-dashboard-audit-resilience.png` as `$5,972.14 capital saved (9.93% of $60.2K notional)`):
```
Across 14 scenarios representing $60163.365 notional, Sentinel preserved $5972.13546111693125 (9.93%); baseline liquidations: 11 -> with Sentinel: 0
```
Bounty wall: seven cards (Perpl API · Perpl Analytics/Risk · Nansen · Qwen · Kimi · Chainlink CRE · Envio) each with
its evidence path — **render at edit time from the README bounty sections; do not fake a screenshot**.
End card: repo URL + the line below; confirm the repo is public before publishing.

**On-screen text:**
```
github.com/JuanLunaIA/Sentinel
every number in this video is in docs/evidence/
```

**Storyboard still:** `docs/evidence/p15-dashboard-audit-resilience.png` (backtest bars + numbers).
Bounty wall / end card: graphic cards (no still exists — must be rendered; never a fake screen).

---

## Recording plan + dry-run results

Per SPEC-P18 §3–4. Dry-runs executed 2026-10-06 by agent Q (this file's author), from `/home/luna/.hermes/Sentinel`,
`LC_ALL=C`. Requirement: both demos run clean twice before any recording — **satisfied: 4/4 exit 0** (plus one
auxiliary verification run with the recording fix applied). Raw logs kept at
`/home/luna/.hermes/cache/scratch/p18-*.log` (ephemeral, ~24 h); key observations are pinned below.

### Dry-run results (all four required runs + auxiliary)

| # | Command | Exit | Wall time | Observed |
|---|---|---|---|---|
| 1 | `bash scripts/crash-demo.sh` | 0 | **193.3 s** | One-off ~2 min rebuild of `target/debug/sentinel` before the daemon started (bin relinked 12:37:48 −03 during the run), then ~75 s replay. 16 `WARN` noise lines (health server bind retries, :8080 held by vaultwarden). All 6 alerts + both reduces printed. |
| 2 | `bash scripts/crash-demo.sh` | 0 | **76.7 s** | Steady state: ~2 s startup + ~75 s replay. Same 16 bind-retry `WARN` lines. Same alert lines, `pipeline finished events=128 executed=2 alerts=6 denied=0`. |
| 3 | `bash scripts/breaker-demo.sh` | 0 | **244.6 s** | One-off release rebuild `Finished … in 2m 46s` (sentinel + breaker), then ~78 s demo. `HTTP 202 {"accepted":true,"epoch":3,"fired":true}`; journal `size 0.635 / notional 999.109`; batch anchored `from_seq=1 entries=57 tx=0x65ca7441…`. Self-cleaned (`/tmp` work dir removed). |
| 4 | `bash scripts/breaker-demo.sh` | 0 | **36.9 s** | Build cached (0.89 s); demo ~36 s (faster than #3: watcher-poll alignment — the 24 s stale window was hit on an earlier poll). `HTTP 202 … fired:true`; same journal values; batch anchored `tx=0xad3a8ff3…`; `heartbeats posted: 2`; exit 0, self-cleaned. |
| aux | `PORT=8091 bash scripts/crash-demo.sh` | 0 | **75.1 s** | Verifies the recording fix: only 2 `WARN` lines (the expected `anchor skipped … PENDING-WALLET` + `feed event channel closed`), no health-bind spam. **Use this on recording day.** |

All runs reproduced the pinned outcomes exactly where they are deterministic (tier sequence, distances
24.10/14.88/7.38%, reduce sizes 2.500/5.000, breaker `size 0.635`, `notional 999.109`, `epoch 3`, `fired:true`).

### Flakes observed + fixes

1. **`:8080` is held by vaultwarden on this dev box** → every daemon start logs `health server bind failed
   error=Address already in use (os error 98)` and the supervisor retries with backoff (noise; non-fatal).
   **Fix (verified, run aux):** `export PORT=8091` before the demo. No dashboard conflict.
2. **First-run rebuilds inflate wall time** (crash #1 +~2 min debug; breaker #1 +2 min 46 s release).
   **Fix:** pre-build before the recording session: `cargo build --bin sentinel && cargo build --release -p breaker`.
3. **Breaker demo wall time varies ~37–80 s** (the fire can come from the breaker's own 5 s auto-path or the
   signed CRE-hop POST; the stale window is a fixed ~24–26 s but watcher polls every 15 s). Budget 2 min per take;
   it retakes cleanly (each run self-cleans its `/tmp` work dir; exit 0 both times).
4. **The demos rewrite their own evidence files on every run** (script design): `scripts/crash-demo.sh` →
   `docs/evidence/p06-crash-demo.txt`; `scripts/breaker-demo.sh` → `docs/evidence/p14-breaker-demo.txt`.
   The breaker transcript's tx hash and `entries=` count depend on local journal state: the pinned transcript
   (`tx=0x390ac28d…`, `entries=4`) was replaced by the dry runs with `tx=0x65ca7441…` / `tx=0xad3a8ff3…`
   (`entries=57`, because `data/audit` had grown). **The pinned file was restored** (md5 `c470c0e2941cbba55b50ec58467a4f7c`)
   so SPEC-P17's citation stays valid; the fresh transcripts are in the scratch logs. Recording day: either preserve/restore
   the pinned file, or use the fresh run's values on screen consistently — **never mix numbers from two runs**.
   (`docs/evidence/p06-crash-demo.clean.txt` was never touched.)
5. **Environment FYI:** a stale `target/debug/breaker` process (pid 11113, from a P14 scratch test, ports 18598/18599)
   was left running before my first run; it does not touch ports 9090/8547 and was left as-is.

### Recording day (Oct 12, operator)

**Preconditions checklist** (SPEC-P18 §3):
- `.env` real keys: Perpl (testnet) · Qwen · Kimi · Telegram → the *live* beats below.
- OBS scenes pre-built: terminal 14pt+ · dashboard fullscreen 1080p60 · Telegram desktop · explorer.
- Record per beat (retry-friendly); captions burned in; upload unlisted YouTube; link into README + submission form.

**If keys are still missing on Oct 12 — exactly what changes:**
- **Hook / SPEED / EVIDENCE:** unaffected (offline replay). Keep the `Monad testnet: PENDING-WALLET` overlay in SPEED.
- **JUDGMENT:** skip the live Telegram consult; show the offline scoreboards (`--mock`, `FORCE_PROVIDER_FAIL=qwen`)
  with overlay `MOCK chain — live keys PENDING-KEY (STUB-01/02)`. Keep the x402 overlay text verbatim:
  *"payment rail verified; first paid call pending wallet"*. Never stage a payment.
- **TRUST:** unchanged (local journal + anvil); keep the `PENDING-WALLET (STUB-17)` explorer overlay. With a funded
  `RPC_SIGNER_KEY`, swap the side-by-side to the real testnet tx (same beat structure).
- **RESILIENCE:** unchanged (fully local); CRE leg keeps the sanctioned `PENDING-ACCOUNT` label + local-runner fallback.

**Runtime tips:** `export PORT=8091`; pre-build; run the breaker demo once before the real take to warm caches;
record the crash replay from a clean `data/audit` if you want the transcript to read like the pinned one.

## Storyboard stills inventory (existing files only)

| Still | Used for |
|---|---|
| `docs/evidence/p15-dashboard-top.png` | Beat 1 (hero) + Beat 4 (audit verifier panel) |
| `docs/evidence/p06-golden-path.png` | Beat 2 (terminal render of the crash log) |
| `docs/evidence/p15-dashboard-positions-feed.png` | Beat 3 (consult stand-in + x402 panel) |
| `docs/evidence/p15-dashboard-audit-resilience.png` | Beat 6 (backtest bars) |
| `docs/evidence/p14-cre-simulate.png` | Beat 5 (CRE leg) |
| `docs/evidence/p15-dashboard-paused-banner.png` | Unused here — available B-roll (kill-switch pause banner) |
| `docs/evidence/p15-dashboard-bottom.png` | Unused — byte-identical scroll-clamp twin of audit-resilience |

## Number → source map (no invented numbers)

| On screen | Source |
|---|---|
| 0.271 µs/iteration | `docs/evidence/p04-skill-verification.txt` |
| distances 24.10% / 14.88% / 7.38%, reduce 25%/50% | `docs/evidence/p06-crash-demo.clean.txt` |
| backtest literal ($60163.365 / $5972.13546111693125 / 9.93% / 11 → 0) | `docs/backtest-report.md` |
| 12/12, 14/14, 2/2 scoreboard | `docs/evidence/p07-brain-eval-mock.txt` |
| 14/14 provider=kimi | `docs/evidence/p08-failover.txt` |
| x402 rails + selected Monad rail + rc=0 | `docs/evidence/p09-x402-check.txt` |
| journal consistent; 5 entries; 5 anchored; root matches at seq 5; tx 0xe67e2d0d… | `docs/evidence/p10-anvil-e2e.txt` |
| breaker 202 fired:true / journal 0.635 / 999.109 / tx 0x390ac28d… | `docs/evidence/p14-breaker-demo.txt` |
| CRE exit 1 + fallback 202 | `docs/evidence/p14-cre-simulate.txt` |
| dashboard values (28.4% / 35.0% / entries 39 / seq #38 / tx 0xac4cd66d41…) | `docs/evidence/p15-dashboard-top.png` (+ `p15-validate.txt`) |
| repo URL | `Cargo.toml` `repository` field (confirm public before publishing) |
