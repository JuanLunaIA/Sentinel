# SPEC-P15 — Dashboard (single-page, served by the daemon)

Frozen: 2026-10-06. Parent-owned; do not edit; report disagreements in open_issues.
Timebox: 5h. Depends: P06 axum surface, P10/P13 APIs (all landed).

## 0. Ownership (disjoint; children never run git)

Agent G (frontend): `dashboard/index.html` ONLY. Agent H (api): `crates/sentinel/src/api.rs` ONLY
(full surface below; P10 audit endpoints keep their behavior; in-file tests). Agent I (verifier):
`crates/sentinel/tests/p15_adversarial.rs` + `tests/fixtures/p15/**`.
Parent prep (committed): `api::DashboardState` + `api::dashboard_router` stub, `spawn_health`
extended (health+journal+kill+live_state) and called in ALL modes incl. replay, placeholder
`dashboard/index.html`. Parent runs the browser pass + screenshots after the wave.

## 1. The page (frozen requirements)

Single file, inline CSS + vanilla JS, ZERO external requests (no CDN, no fonts, no images;
data-URIs max). Dark theme, monospace numerals (font stack ui-monospace/SFMono/Fira Code/mono),
Monad purple accent `#836EF9`. Total size <= 200 KB (wc -c). All dynamic text inserted via
`textContent` (never innerHTML with data). Sections: (1) header wordmark + mode badge + feed
freshness dot + heartbeat age + uptime + version; (2) positions grid with distance-to-liq gauge
(horizontal bar with green->yellow->orange->red zones at thresholds 25/15/8 and a marker) + tier
badge + collateral + uPnL; (3) live decision feed (newest first; trigger icon reflex/strategy/human,
action, market, amount, confidence bar, reason, provider chip, policy verdict chip, tx link when
present, seq + truncated entry_hash with click-to-expand full); (4) audit verifier panel — chain
status, anchored count, latest anchor tx link, running root, live "verify now" button calling
/api/audit/verify; make this panel the visual centerpiece; (5) resilience panel — last heartbeat
(age, tx), breaker status from /api/breaker-status, CRE workflow status line (static text from a
`data-cre` JSON block embedded by the parent later); (6) backtest panel — aggregate capital-saved
stat + per-scenario bar list + reflex latency p50/p99, rendered from GET /api/backtest (bars in
pure CSS); (7) nansen x402 panel — totals, call count, last purchases, budget remaining, cache
stats when present; (8) kill switch — big red button, confirm dialog requires typing PAUSE (and
RESUME for resume), admin key input (session-only, never localStorage), POSTs /api/pause|resume
with ?key=; on success full-page banner "GUARDIAN PAUSED". Poll /api/state + /api/decisions every
2 s; other panels every 15 s; errors render as a muted "unavailable" state, never blank page.

## 2. API surface (frozen JSON; snake_case; decimals as strings)

- `GET /` -> index.html (`include_str!`, path `../../../dashboard/index.html` from api.rs).
- `GET /api/state` -> `{"mode","version","uptime_s","feed_age_s"|null,"feed_fresh":bool,
  "paused":bool,"heartbeat":{"age_s"|null,"tx_hash"|null,"seq"|null},
  "account":{"free_balance","equity"}|null,"positions":[{market_id,symbol,"side":"long|short",
  size,entry_price,mark_price|null,distance_pct|null,liq_price|null,"tier":"green|yellow|orange|
  red"|null,collateral,unrealized_pnl,leverage,"last_action":str|null}]}`.
  Sources: HealthState; LiveState (markets/account/marks/stale_secs/now_ms — compute
  distance/tier via sentinel-core with thresholds 25/15/8 refreshed at the marks); kill flag;
  `data/heartbeat.json` (optional; see SPEC-P16 B3 writer); last_action from the journal tail.
- `GET /api/decisions?limit=` (default 20, cap 100, newest first) -> array of
  `{seq,ts,trigger,market_id,decision|null,policy_verdict|null,execution|null,entry_hash}` from
  `journal.read_entries`.
- `GET /api/audit` + `GET /api/audit/verify` -> EXISTING P10 behavior unchanged.
- `GET /api/backtest` -> raw contents of the P13 report JSON (env `BACKTEST_REPORT_PATH`,
  default `docs/backtest-report.json`); `404 {"error":"backtest report not found"}` when absent.
- `GET /api/nansen/spend` -> `{"total_calls","total_cost_usd","calls_1h","cost_24h_usd",
  "max_calls_per_hour"|null,"recent":[{ts_ms,endpoint,cost_usd,tx_hash|null}] (last 20),
  "cache":{"hits"|null,"misses"|null}}` from the spend ledger at `data/nansen-spend.jsonl`
  (see `nansen::spend::SpendEntry` for the line shape; tolerate absence -> zeros).
- `GET /api/breaker-status` -> `{"available":bool,"state":<data/breaker-state.json>|null,
  "journal_tail":<last 3 parsed lines of data/breaker-journal.jsonl>|[]}`.
- `POST /api/pause` / `POST /api/resume` -> require `?key=` equal to env `DASHBOARD_ADMIN_KEY`
  (constant-time compare). Unset key -> `503 {"error":"admin key not configured"}`; wrong ->
  `401 {"error":"unauthorized"}`; success -> sets the shared kill `AtomicBool` (pause=true,
  resume=false) + best-effort journal SYSTEM entries (errors logged, never fail the response)
  -> `200 {"paused":bool}`.
- CORS: `Access-Control-Allow-Origin: *` + methods/headers on /api/*; mutations remain
  key-gated. Rate limit: hand-rolled per-IP token bucket on /api/* (60 req/min, burst 120;
  IP = first X-Forwarded-For value else socket addr) -> `429 {"error":"rate limited"}`;
  `/healthz` + `GET /` exempt. No new dependencies.

## 3. Router (frozen signature, parent-prep stub)

`pub fn dashboard_router(state: DashboardState) -> Router` with
`DashboardState{health,journal:Option,kill,live_state:Option,p paths:DashboardPaths}` (see stub;
`DashboardPaths::default()` = the literal paths above, `AUDIT_DIR` env honored for `audit_dir`).
`audit_router` stays for compatibility. Parent wires `spawn_health` -> `health::router(...)
.merge(api::dashboard_router(...))` in ALL modes (incl. replay).

## 4. Acceptance / evidence

Parent: daemon in replay + browser screenshots per panel -> `docs/evidence/p15-dashboard-*.png`;
size check `wc -c dashboard/index.html` <= 204800. Verifier: axum test-util black-box for EVERY
endpoint incl. auth paths (401/503), rate limit 429, 404s, shape+type assertions, CORS headers;
HTML static checks (8 section markers/data hooks present, no external URLs — regex for
http(s):// unless in a comment, <= 200KB, no `innerHTML` with template data). `cargo test` green;
clippy -D warnings + fmt clean; report {files_created, tests[{cmd,exit,observed}], open_issues}.

## 4bis. Changelog / adjudications (parent, integration) — v1.0.1

- (a) `feed_fresh` semantics: sourced from the live stream's stale flag (false when
  live_state is absent; true when live and not stale) — pinned by the verifier suite.
- (b) `/api/nansen/spend.recent` is in ledger append order (spec did not freeze order).
- (c) Rate limit: burst 120 / ~60 per min refill; first X-Forwarded-For value is the
  identity; OPTIONS preflight answered 204. main.rs must serve via
  `into_make_service_with_connect_info::<SocketAddr>()` for the socket-addr fallback
  (parent applies the one-line wiring at P16 integration; XFF path already works).
- (d) Tier thresholds are hardcoded 25/15/8 in /api/state (DashboardState carries no
  Config; overriding RISK_*_PCT does not change the dashboard's derived tiers).
- (e) `metrics.reflex_eval_eval` renders 0 µs because the P13 engine emits zeros
  (bin-verified on a fresh run; not an artifact edit). Panel is honest; leave as is.
- (f) Audit panel's anchored seq/tx come from the heartbeat status file (P16 writer);
  `entries`/`valid_up_to_seq` from the live VerifyReport.
- (g) Explorer tx links are assembled at runtime from string parts (static file keeps
  0 http(s):// literals); page load makes no external requests (resource-timing proof).

## 5. Standing rules

Latest versions via live registries; no new deps without parent approval; loud stubs; LC_ALL=C;
deterministic artifacts (no wall-clock in html); children never run git; evidence-shaped reports.
