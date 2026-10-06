# SPEC-P16 — Hardening, Docker & Deployment

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 4h. Depends: P06-P15 core paths.

## 0. Ownership (disjoint; children never run git)

Agent J (docker+runbook): `Dockerfile`, `.dockerignore`, `docker-compose.yml`, `railway.toml`,
`scripts/docker-verify.sh`, `docs/RUNBOOK.md`. Runs builds via `sudo -n docker` (NOPASSWD works);
evidence `docs/evidence/p16-docker.txt`.
Agent K (supervisor): `crates/sentinel/src/supervisor.rs` (NEW), `crates/sentinel/src/main.rs`
(all spawn sites -> supervised), `crates/sentinel/src/notify.rs` (Telegram retry queue) +
`crates/sentinel/tests/p16_resilience_supervisor.rs` (or in-file tests).
Agent L (storage+feed): `crates/sentinel-core/src/audit.rs` (degrade mode),
`crates/sentinel/src/anchor.rs` (heartbeat status file + RPC-outage drain), `crates/sentinel/src/perpl/ws.rs`
(reconnect storm hardening if needed) + `crates/sentinel/tests/p16_resilience_storage.rs`.
Agent M (verifier): `crates/sentinel/tests/p16_adversarial.rs` + `tests/fixtures/p16/**` +
independent re-runs (docker build, compose cycle, secret scan).
Parent: wiring review, deploy attempt documentation, STUBS rows.

## 1. Docker & deploy (J)

`Dockerfile` multi-stage: builder `rust:1.98-bookworm` (toolchain parity with the host; changelog
note for the 1.85 in the prompt), runtime `debian:bookworm-slim` + ca-certificates + curl +
non-root `sentinel` user; builds `--release` the `sentinel` daemon + its bins + `breaker`; copies
`dashboard/`, `contracts/out` not needed, `migrations` none. `HEALTHCHECK CMD curl -f
http://localhost:8080/healthz || exit 1`. `.dockerignore`: target/, .git, data/, node_modules,
indexer/, workflow-cre/node_modules, vendor/. `docker-compose.yml`: service `sentinel` (ports
"8080:8080", env_file .env, volume `./data:/app/data`, restart unless-stopped), service `breaker`
(env_file .env, volume `./data:/app/data`, restart unless-stopped, no ports except BREAKER_PORT if
desired commented). `railway.toml`: dockerfile builder + healthcheck path /healthz + start command
note; a Railway volume must mount /app/data (journal survival) — document the exact `railway
volume`/dashboard steps in RUNBOOK. Deploy itself is PENDING-ACCOUNT (no Railway session
headless): record attempt + STUB-24. Acceptance substitutes: docker build clean + compose up ->
journal seq continuity across a down/up cycle (script `scripts/docker-verify.sh` proves it:
start, run replay producing entries, record last seq, down, up, new entry appends with seq+1).

## 2. Resilience (K, L)

- K supervisor: every `tokio::spawn` in main.rs goes through `supervisor::spawn(name, fut)`:
  panics are caught (JoinHandle inspection), logged with name, restarted with backoff
  (1s..30s cap), counter exposed via tracing; no single task panic kills the daemon. Test:
  task that panics once then succeeds -> daemon stays up, restart logged.
- K notify: `TelegramSink` failures NEVER propagate to the pipeline: `send()` enqueues to a
  bounded queue (32) serviced by a background task with retry 3x exponential (500ms base);
  overflow drops oldest + warn. Test with wiremock 500 -> no Err to caller, retries observed.
- L audit degrade: `AuditJournal` persistence failures (disk full / read-only) degrade to
  in-memory + `degraded()` flag + retry persistence on the next append; in-flight order flow
  never blocks; alarm via tracing::error once per transition. Test: journal dir chmod 0o555 ->
  record succeeds in memory, degraded true; chmod back -> next append persists (both entries
  appear). verify_chain untouched.
- L anchor: after every successful beat/batch write `data/heartbeat.json`
  `{"ts_ms":u64,"tx_hash":str|null,"seq":u64}` (atomic tmp+rename). RPC outage: bogus RPC URL ->
  queue/retry with backoff, journal unaffected; restore (anvil) -> drains. Test with anvil
  stop/start via `anvil` on an ephemeral port.
- L ws storm: `perpl/ws.rs` reconnect survives 10 consecutive kill cycles against a local
  tokio-tungstenite test server (backoff honored, final event delivered, no panic).

## 3. Security pass (K, M)

- M secret-scan test: scan `docs/evidence/*.txt`, `data/*.log` samples + one generated sample
  log for the NON-PLACEHOLDER secret values from `.env` (if present) and known dev keys; assert
  absent. Placeholders ("replace-me" prefixes) exempt. Also scans for "PRIVATE KEY" payloads
  outside redacted markers.
- K redaction audit test: `Debug` of Config / NansenConfig / PerplConfig / ApiKeySigner never
  contains the underlying secret values (`SecretString` / typed wrappers; add impls where
  missing).
- x402 low-balance alert (< $1): DEFERRED (needs live payer wallet; note in SPEC + STUB).
  Budget warnings: existing budget guard logs at 80% (verify + pin a test if cheap).
- Rate limit + admin key: implemented in P15 (api.rs); M re-verifies 429/401/503.

## 4. Acceptance / evidence

`docs/evidence/p16-docker.txt`: docker build tail (no errors) + compose cycle + seq continuity;
`docs/evidence/p16-validate.txt`. `scripts/crash-demo.sh` still passes locally (re-run).
RUNBOOK.md: start/stop, mode switching, key rotation, journal recovery, breaker arming,
what-to-do-if table, Railway deploy steps (volume + env). Deploy = PENDING-ACCOUNT (STUB-24).
`docs/evidence/p16-deployed.png` deferred (blocked) — document.

## 4bis. Changelog / adjudications (parent, integration) — v1.0.1

- (a) Supervisor signature: frozen brief said `spawn(name, make_fut)`; implemented
  `spawn(name, shutdown, make_fut)` — the watch is required for the frozen
  'no restart after shutdown' semantics and its test. All main.rs sites converted;
  zero `tokio::spawn` remains.
- (b) Pipeline run stays awaited (its feed/executor/sink are non-Clone: a restart
  cannot rebuild them); a pipeline panic remains fatal (pre-existing), the four
  auxiliary servers are supervised.
- (c) Supervision deltas: health server retries bind failures (1s..30s backoff);
  bot rebuilds context per attempt with the ApprovalQueue hoisted (survives restarts);
  `TelegramSink::send` now queues and returns Ok always (breaker included).
- (d) Docker builder `rust:1.98-bookworm` (latest-rule over the prompt's 1.85);
  builds need `--network=host` on this host (nftables forward-drop) and the legacy
  builder has no layer caching (SENTINEL_SKIP_BUILD=1 for cycle-only re-runs).
- (e) One writer per journal: concurrent compose cycles over one `./data` fork the
  hash chain — incident observed during verification (two writers, broken at seq 41),
  repaired to the longest valid prefix, disclosed, re-run clean; RUNBOOK §5 carries
  the warning.
- (f) `data/heartbeat.json` `{"ts_ms","tx_hash"|null,"seq"}` written atomically after
  each successful beat (seq = journal head) and batch anchor (seq = last seq covered);
  `HEARTBEAT_PATH` env overrides the path; failed beats retry with 1s..30s backoff.
- (g) Telegram 5xx wall-clock: teloxide delays ~10 s per attempt on server errors
  (~45 s to abandon); connection-level failures use the 0.5 s base. Documented in RUNBOOK.
- (h) Railway deploy remains PENDING-ACCOUNT (STUB-24); docker build + compose cycle +
  seq continuity + in-image dashboard/audit-verify are the standing proofs.
- (i) x402 low-balance (< $1) alert DEFERRED until a live payer wallet exists (STUB-25);
  budget warnings already log at the guard's thresholds.
- (j) Secret scan is non-vacuous via negative controls but currently enforces zero real
  values (all placeholders); it strengthens automatically when real keys land.
- (k) `perpl/ws.rs` needed no change: the 10-kill storm test is the evidence for the
  existing reconnect+backoff logic.

## 5. Standing rules

Latest versions via live registries; loud stubs; no secrets in any artifact; LC_ALL=C;
children never run git; evidence-shaped reports {files_created, tests[{cmd,exit,observed}], open_issues};
clippy -D warnings + fmt + full cargo test are gates.
