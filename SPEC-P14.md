# SPEC-P14 — CRE dead-man's-switch workflow + breaker service

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 5h. Depends: P10 (heartbeat contract + anchor task; contract at `contracts/src/SentinelAuditAnchor.sol`),
installed CRE CLI v1.37.0 (`~/.local/bin/cre`, see FACTS.md §5; per-tenant enablement = STUB-03).

## 1. File ownership (disjoint; children never run git)

Agent D (breaker-core): `crates/breaker/src/{main.rs, config.rs, watcher.rs, snapshot.rs, trigger.rs,
executor.rs, armed.rs}` (+ its own unit tests inside those files). Parent preps the crate skeleton + workspace member.
Agent E (workflow+demo): `workflow-cre/**`, `scripts/breaker-demo.sh`, `docs/evidence/p14-cre-*`, `docs/evidence/p14-breaker-demo.txt`.
Agent F (verifier): `crates/breaker/tests/breaker_adversarial.rs`, `docs/evidence/p14-validate.txt`.
Parent: root `Cargo.toml` member entry, breaker stub skeleton, replay-mode anchor spawn in
`crates/sentinel/src/main.rs`, `.env.example` BREAKER_* block, STUBS.md rows.

## 2. BreakerConfig (env; exact keys + defaults)

BREAKER_RPC_URL (default http://127.0.0.1:8545) | BREAKER_ANCHOR_ADDRESS (required) |
BREAKER_GUARDIANS (csv 0x addresses; required) | BREAKER_HEARTBEAT_INTERVAL_SECS (60) |
BREAKER_STALE_MULT (3) | BREAKER_MODE dry_run|testnet (dry_run) | BREAKER_FRACTION ("0.5") |
BREAKER_MAX_REDUCE_USD ("1000") | BREAKER_PORT (9090) | BREAKER_ARM_SECRET (required for POST) |
BREAKER_SNAPSHOT_FILE (optional) | BREAKER_STATE_FILE (data/breaker-state.json) |
BREAKER_JOURNAL (data/breaker-journal.jsonl) | PERPL_API_KEY/PERPL_API_URL (testnet mode) |
TELEGRAM_BOT_TOKEN/TELEGRAM_APPROVAL_CHAT_ID (optional alerts).

## 3. Watcher / staleness / idempotency (frozen)

- Watcher: alloy provider `eth_getLogs` on the anchor contract, topic0 = Heartbeat
  (`Heartbeat(address guardian, bytes32 riskStateHash, uint32 openPositions, uint8 maxTier)`),
  lookback blocks configurable (default 7200, poll every 15 s). Track latest event per guardian.
- Tier mapping (matches contract): Green=0, Yellow=1, Orange=2, Red=3.
- stale <=> age_secs > stale_mult * heartbeat_interval (STRICT >; boundary case 3x exactly = NOT stale).
- critical <=> max_tier >= 2 (Orange or Red). Fire = stale AND critical.
- Idempotency: at most one fire per (guardian, epoch), epoch = floor((now_ms - last_ts_ms)/interval_ms).
  Persisted atomically (tmp+rename) to BREAKER_STATE_FILE; a fresh heartbeat (new last_ts) resets.

## 4. Action executor (frozen)

Guard -> snapshot source order: (1) live Perpl REST if PERPL_API_KEY/PERPL_API_URL present;
(2) BREAKER_SNAPSHOT_FILE (JSON array of Position); (3) none => alert-only, NO order (safe degrade).
Pick riskiest position: smallest distance_to_liq_pct; tie -> larger |size|*mark notional.
Size: fraction * |size|; clamp DOWN so notional <= BREAKER_MAX_REDUCE_USD; quantize with core
sizing; if < market.min_size => alert-only. Execution: dry_run => reuse
`sentinel::execution::dry_run::DryRunExecutor` (fill at mark +/- slippage, journal);
testnet => `sentinel::execution::perpl::PerplExecutor` directly (no GuardedExecutor; breaker
re-verifies by re-reading positions and journaling; this is a last-resort path, documented).
Journal JSONL line: {ts_ms, guardian, epoch, mode, market_id, fraction, size, notional_usd,
status, detail}. Alert via `sentinel::notify::TelegramSink` when configured else tracing::warn;
message starts with the frozen literal "BREAKER: Sentinel unresponsive" (+prefixed emoji).

## 5. Armed HTTP surface (frozen)

GET /api/heartbeat-status -> 200 {"generated_at_ms":n,"guardians":[{"address","last_ts_ms",
"age_secs","max_tier","stale","critical","armed"}]} ("armed" = would fire now or already fired
this epoch). POST /breaker/trigger: body exactly {"guardian":"0x..","reason":"..","requested_at_ms":n};
header `X-Breaker-Signature: sha256=<hex HMAC_SHA256(secret, raw request body bytes)>`; constant-time
compare; 202 {"accepted":true,"epoch":n,"fired":bool} (fired=false on duplicate epoch); 401 bad
signature; 400 bad body; 423 when the guardian is fresh (stale=false).
Spec test vector (secret b"spec-test-secret", body bytes as below):
body = {"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","reason":"spec-vector","requested_at_ms":1791200000000}
signature = sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04

## 6. workflow-cre (TS, per docs.chain.link/cre + installed CLI v1.37.0)

Folder `workflow-cre/sentinel-deadman/` (structure the CLI accepts; probe `cre workflow --help`,
`simulate --help`, and docs.chain.link/cre first). Logic: cron trigger every 2 min -> HTTP GET the
breaker's /api/heartbeat-status (and, if straightforward, a direct RPC-read leg) -> condition:
any guardian stale && critical -> target step: HTTP POST /breaker/trigger (HMAC secret via CRE
secrets/config) + optional Telegram alert. Deliverables: workflow definition + main.ts +
config/README. Evidence: `cre workflow simulate` transcript with an injected stale state ->
docs/evidence/p14-cre-simulate.txt (+png if a TTY render works). Registration/broadcast: attempt
per CLI capabilities on the chain where the anchor lives; if tenant/login blocks it (STUB-03),
record exactly and fall back per the prompt (definition + emulation, clearly labeled).

## 7. scripts/breaker-demo.sh (unattended; frozen shape)

(0) build breaker+sentinel; (1) anvil --silent on 8547 + `forge create SentinelAuditAnchor` from
contracts/ (anvil key #0), export address; (2) daemon: replay mode on
tests/fixtures/perpl/crash-scenario.jsonl with the parent's replay-anchor env (ANCHOR_RPC_URL,
ANCHOR_CONTRACT_ADDRESS, HEARTBEAT_INTERVAL_SECS=8); (3) breaker: dry_run watching anvil,
interval 8s, snapshot file tests/fixtures/p14/breaker-snapshot.json (agent E creates it from the
crash scenario's positions; positions file owned by agent E); (4) show /api/heartbeat-status with
stale=false; (5) `kill -9` the daemon; (6) poll until stale (>24 s) -> fired -> journal line +
alert printed; (7) summary + cleanup traps (kill all; no docker). Exit 0; transcript ->
docs/evidence/p14-breaker-demo.txt.

## 8. Acceptance / validation

- breaker unit tests: staleness boundaries (== 3x NOT stale), idempotency (one fire per epoch,
  restart persistence), HMAC (valid/invalid/tampered body), clamp/min-size degrade, dry-run flow,
  snapshot degrade order. `cre workflow simulate` exit 0 (or documented fallback).
- breaker-demo.sh completes unattended with exit 0 and the transcript shows the full sequence.
- Gates: clippy `-D warnings` (tests included) + rustfmt; evidence-shaped reports; no git from
  children; latest versions via live registries; loud stubs; LC_ALL=C determinism.

## 8bis. Changelog / adjudications (parent, integration) — v1.0.1

- (a) Refire semantics: fires once per NEW epoch while staleness persists (the §3
  formula literally; a fresh heartbeat starts a new generation). Accepted as frozen;
  the demo captures the first fire.
- (b) Auto-fire loop (5 s tick, first check delayed one interval) is the §7 demo path,
  wired in main.rs; the armed POST shares the same persisted epoch gate.
- (c) BREAKER_ARM_SECRET is required at config load (POST surface always mounted);
  testnet mode additionally needs PERPL_API_KEY_SECRET (PerplExecutor signing).
- (d) BREAKER_SNAPSHOT_FILE accepts the spec-literal Position[] AND the extended
  {"positions":[...],"markets":[...]} object; without market metadata, sizing uses the
  documented Perpl fallback (size_decimals=3, min_size=0, FACTS §1.5/§1.7) recorded in
  journal details. Journal decimals serialize as JSON numbers; alert-only lines carry 0.0.
- (e) Watcher staleness detection can lag up to the 15 s poll interval.
- (f) §7 reconciliation: the daemon's anchor leg reads `PERPL_RPC_URL` (anchor.rs),
  not `ANCHOR_RPC_URL` — the demo exports PERPL_RPC_URL + BREAKER_RPC_URL; wording fixed here.
- (g) breaker-demo.sh launches the binaries built in step (0) (unattended runs must not
  block on the shared cargo build lock); `P14_CARGO_RUN=1` restores the literal
  `cargo run` form.
- (h) CRE simulate is tenant-gated: `cre workflow simulate`/`supported-chains` exit 1
  with 'Authentication required' headless (STUB-03, PENDING-ACCOUNT). Offline proof
  executed per the §6 fallback: `cre workflow build` exit 0 (binary hash
  904b5f68…), `cre workflow hash` workflow hash 00cb333e…, and tools/local-runner.ts
  over the same src/core.ts definition drove the SPEC §5 HMAC vector + a full
  stale-emulator→chain-read→signed-POST→202 sweep. Prereqs pinned: bun on PATH;
  typescript 5.9.3 (npm latest 7.0.2 breaks cre-compile validate).
- (i) `crates/breaker/src/lib.rs` added as crate root (integration tests import the
  breaker API); main.rs depends on the lib target.

## 9. Parent prep (already in prep commit)

Workspace member `crates/breaker` + skeleton; `main.rs` replay-anchor spawn; `.env.example`
BREAKER_* block; STUB-22/23 rows at integration.
