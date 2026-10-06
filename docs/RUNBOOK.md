# Sentinel — Operations Runbook (P16)

Operator-facing procedures for running, dockerizing and (eventually) deploying
Sentinel. Evidence for this phase: `docs/evidence/p16-docker.txt` (docker
build + compose cycle + journal seq continuity), `docs/evidence/p16-validate.txt`.

Scope: the `sentinel` daemon, `breaker`, the audit journal, and the two
distribution paths (docker compose locally, Railway remote — Railway itself
is **PENDING-ACCOUNT**, STUB-24: no headless session).

---

## 0. Surfaces at a glance

| Surface | Where | Notes |
|---|---|---|
| `GET /healthz` | `PORT` (default `8080`) | `{uptime_s, feed_age_s, mode, version}`; `feed_age_s` is `null` until the first feed event |
| `GET /api/audit`, `GET /api/audit/verify` | same server (P10) | journal windows + chain verify report |
| `GET /api/state`, `/api/decisions`, `/api/backtest`, `/api/nansen/spend`, `/api/breaker-status` | same server (P15) | dashboard panels |
| `POST /api/pause`, `POST /api/resume` | same server (P15) | kill switch mutations — require `DASHBOARD_ADMIN_KEY`; **unset ⇒ mutations disabled** |
| Breaker armed surface | `BREAKER_PORT` (default `9090`) | `GET /api/heartbeat-status`, `POST /breaker/trigger` (HMAC) |
| Audit journal | `data/audit/journal-YYYYMMDD.jsonl` | append-only hash chain, seq starts at 0 |
| Breaker state/journal | `data/breaker-state.json`, `data/breaker-journal.jsonl` | one-fire-per-epoch gate + fired history |
| Anchor heartbeat status | `data/heartbeat.json` | `{ts_ms, tx_hash \| null, seq}` written by the anchor task |

The daemon and the breaker are **separate processes**; compose runs both from
the same image.

---

## 1. Start / stop

### 1.1 Local (cargo, no docker)

```bash
cd /path/to/Sentinel
cp .env.example .env         # placeholders pass validation for smoke runs
./target/release/sentinel                  # mode from EXECUTION_MODE
./target/release/sentinel --mode dry-run   # one-shot override
./target/release/sentinel --mode dry-run \
    --replay tests/fixtures/perpl/crash-scenario.jsonl   # deterministic replay
```

Stop: `Ctrl-C` (SIGINT) or `kill -TERM <pid>` — the daemon drains the pipeline
and flushes telemetry. Do **not** `kill -9` outside crash rehearsals.

> **Live-mode credentials requirement (verified P16):** DRY_RUN *live* and
> TESTNET modes fetch a signed account snapshot at startup. With placeholder
> credentials the daemon **exits** at the first signed REST call
> (`GET /v1/trading/wallet → HTTP 401`). `--replay` is the fully offline mode;
> use it for smoke tests. This is why `scripts/docker-verify.sh` pins the
> compose service to a replay (see §4).

Logs: `LOG_DIR` (default `./logs`), file `sentinel.YYYY-MM-DD.log`; bump
verbosity with `RUST_LOG=sentinel=debug`.

### 1.2 Docker (single container, compose not required)

Build once (the UID/GID args let the non-root container user write the
bind-mounted `./data`; `--network=host` is required on *this* host because a
system nftables `forward` policy `drop` blocks container-bridge egress — see
§8):

```bash
docker build --network=host \
  --build-arg SENTINEL_UID=$(id -u) --build-arg SENTINEL_GID=$(id -g) \
  -t sentinel:p16 .
```

Run the daemon (live config from `.env`):

```bash
docker run --rm -p 8080:8080 --env-file .env \
  -v "$PWD/data:/app/data" sentinel:p16
```

Run a deterministic replay instead (safe without venue credentials):

```bash
docker run --rm --env-file .env -v "$PWD/data:/app/data" sentinel:p16 \
  sentinel --mode dry-run --replay tests/fixtures/perpl/crash-scenario.jsonl
```

Stop: `docker stop <container>` (SIGTERM → graceful drain; 10 s grace).

### 1.3 Docker compose (recommended)

```bash
docker compose up -d          # sentinel + breaker
docker compose ps
docker compose logs -f sentinel
docker compose down           # stop + remove (the ./data volume persists)
```

- `sentinel`: daemon; host `8080:8080`; `/healthz` reachable at
  `http://localhost:8080/healthz`.
- `breaker`: same image, `command: ["/usr/local/bin/breaker"]`; no host ports
  (uncomment the `BREAKER_PORT` mapping in `docker-compose.yml` if needed).
- Both use `env_file: .env` and share `./data:/app/data`; `restart: unless-stopped`.
- Apply `.env` changes: `docker compose up -d --force-recreate`.
- Rebuild after source changes: re-run the `docker build` command above, then
  `docker compose up -d --force-recreate`.

**Port conflicts.** If `8080` is already taken on the host (it is on the dev
box — vaultwarden), either stop the other service or add a local
`docker-compose.override.yml`:

```yaml
services:
  sentinel:
    ports: !override
      - "127.0.0.1:18080:8080"
```

(The verify script does exactly this automatically in a temp overlay;
`SENTINEL_HOST_PORT` selects the host port.)

**Breaker env.** The breaker **fail-fasts** on missing `BREAKER_ANCHOR_ADDRESS`,
`BREAKER_GUARDIANS` or `BREAKER_ARM_SECRET` (exit + compose restart loop).
Fill them in `.env` before enabling the service, or run it only through
`scripts/breaker-demo.sh` (which supplies demo values — §6).

---

## 2. Mode switching

Mode lives in one of: `.env` `EXECUTION_MODE`, a `--mode <value>` CLI flag
(one-shot override), or the effective policy overlay (kill switch).

| Mode | Requirements | Behavior |
|---|---|---|
| `DRY_RUN` (default) | valid venue credentials (startup snapshot) | live feed, simulated fills |
| `TESTNET` | `PERPL_ENV=testnet` + API key/secret + ACCOUNT | real reduce-only orders via the guarded gateway |
| `MAINNET` | `PERPL_ENV=mainnet` + `I_UNDERSTAND_MAINNET_RISK=yes` + account | **refused**: "mainnet is not wired yet" — does not start |
| `--replay` (flag) | none (offline) | deterministic fixture + DRY_RUN executor + logical clock |

To switch: edit `EXECUTION_MODE` in `.env` → restart (local: SIGTERM + start;
compose: `docker compose up -d --force-recreate sentinel`; Railway: variables
→ redeploy). Validation rejects mismatches (e.g. TESTNET mode with
`PERPL_ENV=mainnet`) at startup — check the startup log line
`sentinel starting mode=… perpl_env=…`.

---

## 3. Key rotation

All secrets are read **at process start** (`Config::load` / `BreakerConfig::from_env`);
there is no hot reload. Rotate = update the value in `.env` (or Railway
variables) and **restart the affected process** (`docker compose up -d
--force-recreate sentinel|breaker`; local SIGTERM + start; Railway: redeploy
after editing variables). Secrets never appear in logs (`SecretString`
redaction — tested in P16 §3).

| Secret (env) | Scope | Rotation procedure |
|---|---|---|
| `PERPL_API_KEY` + `PERPL_API_KEY_SECRET` | venue auth (REST + WS signing) | create the new pair at `https://testnet.perpl.xyz/apikeys`; keep the old key valid until the new one passes a live start; then update `.env` + restart. Both must rotate together (secret is shown once). |
| `QWEN_API_KEY`, `KIMI_API_KEY` | strategy brain providers | issue new key at the provider; update + restart. Model/budget knobs (`QWEN_MODEL`, `STRATEGY_*`) ride the same restart. |
| `TELOXIDE_TOKEN` | Telegram bot | revoke/issue via BotFather; update + restart. Revoking invalidates the old token immediately (bot goes silent until restart). |
| `NANSEN_PAYER_KEY` (+`NANSEN_MAX_CALLS_PER_HOUR`) | x402 payer wallet | move funds to the new address, update + restart; the spend ledger (`data/nansen-spend.jsonl`) is address-independent. |
| `RPC_SIGNER_KEY` (+ `ANCHOR_CONTRACT_ADDRESS`) | on-chain anchor signer (Monad) | fund the new address with MON first; update + restart. The contract tracks anchors **per signer** (seq contiguous from 1 for each) — rotation starts a new seq run, and `audit-verify` cross-checks per signer. |
| `BREAKER_ARM_SECRET` | HMAC for `POST /breaker/trigger` | update `.env` + restart the breaker, and update the signer (CRE workflow / ops script) in the same window; old signatures are rejected after restart. |
| `DASHBOARD_ADMIN_KEY` | `/api/pause`, `/api/resume` mutations | set/rotate + restart. Unset ⇒ mutations disabled (safe default). |

---

## 4. Docker acceptance / verification

`scripts/docker-verify.sh` proves the P16 §1 acceptance substitute:

```bash
scripts/docker-verify.sh          # full run (build + compose cycle), evidence transcript
SENTINEL_SKIP_BUILD=1 scripts/docker-verify.sh   # re-run the cycle reusing sentinel:p16
```

What it does: builds the image → `docker compose up -d` (auto-selects host
port `18080` when `8080` is busy; `SENTINEL_HOST_PORT` overrides) → waits for
`/healthz` → the daemon **replays** the crash scenario inside the container
(host-side overlay pins the service command; entries journal to the mounted
`./data`) → `docker compose down` → records the last journal seq → `up` again
→ asserts the first appended entry is `seq = old + 1` → runs `audit-verify
--no-chain` on the stable journal. Full transcript:
`docs/evidence/p16-docker.txt`.

---

## 5. Journal recovery

The journal is the audit spine: append-only JSONL, `prev_hash`/`entry_hash`
chained, `seq` contiguous from 0. Verify it with the bundled CLI (works
offline; also available inside the image):

```bash
cargo run --release --bin audit-verify -- --no-chain          # local chain only
audit-verify --journal data/audit/journal-20261006.jsonl      # explicit file (chain cross-check needs RPC + contract)
audit-verify --no-chain                                       # inside the container image
```

Healthy: `✅ journal consistent; {N} entries; {M} anchored; root matches at seq {X}` (exit 0).
Broken: `❌ broken at seq {S}: {detail}` (exit 1).

| Finding | Meaning | Action |
|---|---|---|
| `ignored torn trailing line N` | crash mid-write; only the very last line is partial | none needed — the daemon resumes from the last valid entry (skip). If the daemon already appended after it, the line is now **interior**: stop the daemon, back up the file, delete only that one malformed line, re-verify, restart. |
| `prev_hash mismatch at seq S` | chain discontinuity (tamper, mixing files, manual edit) | STOP writes. Keep a copy (`cp` → `journal-….quarantine-<ts>`). Cross-check anchors (`audit-verify` with `RPC`/contract, or the indexer GraphQL). Do not hand-edit hashes. Restore from a trusted backup if available; otherwise document the break and keep the file as evidence. |
| `entry_hash mismatch at seq S` | entry content altered | same as above — the pair (content, hash) is the tamper detection; never "fix" by rewriting hashes. |
| `malformed journal line N` (interior) | partial line buried by later appends | see torn-tail repair above. |

Crash rehearsals that exercise kill/resume: `scripts/p10-crash-rehearsal.sh`
(evidence `docs/evidence/p10-crash-safety.txt`).

> **One writer per journal.** Never run two daemons — or two compose stacks /
> concurrent verification cycles — against the same `./data` at the same time:
> each process resumes from the file head independently, so concurrent
> appends interleave two hash chains and `audit-verify` reports a
> `prev_hash mismatch` (observed in P16: duplicate `seq` values with different
> hashes, ~0.3 s apart). Repair: stop all writers, keep a copy of the file,
> truncate to the longest chain-valid prefix, re-verify, then restart exactly
> one writer.

> Note (P16 verification): Telegram 5xx retries carry teloxide's own 10 s
> server-error delay per attempt (~45 s to abandon); connection-level failures
> retry at the notify queue's 0.5 s base. `HEARTBEAT_PATH` (default
> `data/heartbeat.json`) overrides the anchor heartbeat status file consumed by
> the dashboard's resilience panel.

**Degraded journal (P16 §2).** If persistence fails (disk full, read-only),
the journal **degrades instead of failing**: entries are kept in memory, one
`audit journal persistence failed; degrading to in-memory …` error is logged
on the transition, and the next append retries flushing everything in seq
order (`persistence recovered; in-memory entries flushed`). Reads (`/api/audit`)
include the in-memory tail. While degraded, entries are **not durable** —
resolve the disk problem promptly and re-run `audit-verify` after recovery.

---

## 6. Breaker arming (dead-man's switch, P14)

Required env (fail-fast): `BREAKER_ANCHOR_ADDRESS`, `BREAKER_GUARDIANS` (csv),
`BREAKER_ARM_SECRET`. Optional: `BREAKER_RPC_URL` (default
`http://127.0.0.1:8545`), `BREAKER_HEARTBEAT_INTERVAL_SECS` (60),
`BREAKER_STALE_MULT` (3), `BREAKER_MODE` (`dry_run` | `testnet`),
`BREAKER_FRACTION`, `BREAKER_MAX_REDUCE_USD`, `BREAKER_PORT` (9090),
`BREAKER_STATE_FILE`, `BREAKER_JOURNAL`, `BREAKER_SNAPSHOT_FILE`.

Start it via compose (§1.3) or standalone: `./target/release/breaker`.
Arming/behavior is self-gating: a guardian fires only when its last anchor
`Heartbeat` is **stale** (`age > stale_mult × interval`) **and** critical, one
fire per epoch (persisted gate shared by the 5 s auto path and the armed POST).

Demo (full sequence incl. anvil): `scripts/breaker-demo.sh` → evidence
`docs/evidence/p14-breaker-demo.txt`. Manual fire (the CRE hop):

```bash
BODY='{"guardian":"0xf39F…92266","reason":"sentinel-unresponsive-heartbeat-stale-critical","requested_at_ms":1728000000000}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$BREAKER_ARM_SECRET" -hex | awk '{print $NF}')
curl -X POST http://127.0.0.1:9090/breaker/trigger \
  -H 'Content-Type: application/json' -H "X-Breaker-Signature: sha256=$SIG" \
  --data-raw "$BODY"      # → 202 {"fired":true, …}
curl -s http://127.0.0.1:9090/api/heartbeat-status | jq .
```

`BREAKER_MODE=dry_run` (default) never places real orders; `testnet` uses the
documented last-resort reduce path.

---

## 7. Railway deploy (PENDING-ACCOUNT — STUB-24)

`railway.toml` (repo root, config-as-code) already sets: DOCKERFILE builder →
`Dockerfile`, deploy healthcheck `GET /healthz` (120 s timeout), restart
`ON_FAILURE` × 10. The image CMD runs the daemon; Railway injects `PORT` and
the daemon binds it.

Steps (after `railway login` — interactive; not possible headless):

1. **Project**: `railway init --name sentinel` (new project) or
   `railway link` and pick project/environment/service.
2. **Volume (REQUIRED — journal survival)**: attach a volume at `/app/data`
   *before the first real deploy*:
   `railway volume add --mount-path /app/data`
   (Dashboard alternative: service → **Data** → **Add Volume** → mount path
   `/app/data`.) Without it, the audit journal + breaker state reset on every
   redeploy. Railway's own guidance for relative-path apps is exactly this
   mount (`./data` → `/app/data`).
3. **Variables** (from `.env.example` — never commit real secrets):
   Dashboard → service → **Variables** → Raw Editor, or
   `railway variables --set KEY=VALUE …`. Required set: `PERPL_ENV`,
   `PERPL_API_KEY`, `PERPL_API_KEY_SECRET`, `PERPL_ACCOUNT`, `EXECUTION_MODE`
   (`DRY_RUN` first), `QWEN_API_KEY` (+`KIMI_API_KEY` fallback), `TELOXIDE_TOKEN`
   + `TELEGRAM_ALLOWED_USER_IDS`, risk knobs (`RISK_*`, `MAX_*`, …). Do **not**
   set `PORT` (Railway injects it). `ANCHOR_*`/`RPC_SIGNER_KEY` and `BREAKER_*`
   only once those wallets/secrets exist.
4. **Deploy**: `railway up` (Dockerfile built remotely; builds have normal
   internet — the local nftables caveat in §8 does not apply). Watch
   `railway logs`; the deploy passes only when `/healthz` answers within 120 s.
5. **Post-deploy checks**:
   - `railway logs | grep "health + audit API"` — server bound;
   - `railway volume files list /` — `data/audit/` appears once entries journal;
   - optional: `railway domain` → `curl https://<domain>/healthz`.
   - If logs show `audit journal persistence failed; degrading to in-memory …`,
     the volume is mounted but not writable by the container user (uid 10001
     default on Railway builds): rebuild with `SENTINEL_UID/GID` matching the
     volume owner or run with a root-init chown wrapper — coordinate with the
     account holder.

Acceptance for this phase is **documentation only**: the deploy itself is
blocked on an interactive Railway account (`docs/evidence/p16-deployed.png`
deferred; STUB-24 in `docs/STUBS.md`).

---

## 8. What to do if …

| Symptom | Where you see it | What to do |
|---|---|---|
| **Feed stale** | alerts `⚠️ feed stale: no data for Ns`; `/healthz` `feed_age_s` climbing | check venue/network reachability (`PERPL_WS_URL`); the WS layer reconnects forever with backoff ≤ 30 s and emits `Reconnected{attempt}` — no action unless it never recovers. Verify credentials if the log shows 401s. On this host, containers need internet egress: bridge egress is currently blocked by the system nftables `forward` policy (indexer/README.md) — run with `--network=host` or add the narrow forward rule. Replay is unaffected. |
| **Breaker misfire** (fired while Sentinel was healthy) | `data/breaker-journal.jsonl` new line; alert `BREAKER: Sentinel unresponsive` | inspect `GET /api/heartbeat-status` (age vs `stale_mult × interval`): check clock skew, RPC health (`BREAKER_RPC_URL`) and whether the anchor task was posting heartbeats (`data/heartbeat.json` freshness). The fire is one-per-epoch and persists; switching `BREAKER_MODE=dry_run` (default) prevents real orders. Fix the heartbeat path, then bump/replace state to allow the next epoch to fire normally. |
| **Journal degraded** | `audit journal persistence failed; degrading to in-memory` (error, once) | entries still recorded in memory + served; fix the cause (next row), then wait for `persistence recovered; in-memory entries flushed`; re-run `audit-verify --no-chain` to confirm the chain. |
| **Disk full** | degraded journal; log-write errors; `ENOSPC` in logs | `df -h`; prune docker (`docker system prune -af`, `docker image prune`); rotate `logs/`; move `data/` to a larger volume (stop daemon first, keep the journal copy). The journal flushes automatically on the next append after space frees. |
| **RPC down** (anchor / breaker RPC) | anchor warns; breaker `first heartbeat poll failed…` | anchor: queue/retry with backoff, journal unaffected; breaker: it keeps serving with its last state — if RPC stays down the breaker may see guardians as stale; until RPC returns, set `BREAKER_MODE=dry_run` (default anyway) or stop the breaker service. `audit-verify --no-chain` is the offline verification path. |
| **Telegram dead** | bot silent; alerts lacking | log in as the bot holder: check `TELOXIDE_TOKEN` (revoked? replaced?), `TELEGRAM_ALLOWED_USER_IDS`; the notify layer never blocks the pipeline (bounded queue, retries, oldest-drop on overflow) — alerts still reach `logs/`. Rotate the token per §3 and restart. |

---

## 9. Standing operator constraints

- Never commit `.env` (or any real secret); the docker context and image
  exclude it (`.dockerignore`). Evidence artifacts must contain no secret
  values (scanned in P16 §3).
- Docker on this box: run via `sudo -n docker …` (user not in the `docker`
  group; socket relay fallback in `indexer/README.md`).
- `LC_ALL=C` for scripts/evidence.
- Mainnet is deliberately not wired: do not attempt `--mode mainnet`.
