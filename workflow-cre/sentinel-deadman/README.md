# sentinel-deadman — CRE workflow (SPEC-P14 §6)

Cron (every 2 min) ➜ `GET /api/heartbeat-status` ➜ condition *(any guardian
`stale && critical`)* ➜ optional chain-read corroboration ➜ signed
`POST /breaker/trigger` ➜ optional Telegram alert.

```
sentinel-deadman/
├── workflow.yaml            # CRE targets: local-simulation / production-settings
├── main.ts                  # entry point: Runner + initWorkflow (configSchema-validated)
├── src/core.ts              # runtime-agnostic logic shared with the fallback runner:
│                            #   status parsing, fire condition, trigger body, HMAC
│                            #   signature, JSON-RPC read leg, alert text, runSweep()
├── src/workflow.ts          # CRE glue: HTTPClient steps + cron handler (imports cre-sdk)
├── config.local.json        # local anvil + emulated breaker (port 9091)
├── config.production.json   # Monad-testnet template (placeholder values, loud)
├── tools/local-runner.ts    # FALLBACK: emulates simulate with the same core.ts
├── tools/stale-breaker-emulator.ts  # deterministic stale-state breaker endpoint
└── tools/cre-simulate-attempt.sh    # `cre workflow simulate` wrapper + verbatim attempt
```

## Definition (what runs, step by step)

1. **Trigger** — `cre.handler(new CronCapability().trigger({schedule}), onCronTrigger)`,
   schedule from config (`0 */2 * * * *` = every 2 minutes, SPEC §6).
2. **Step 1** — HTTP GET `<breaker.baseUrl>/api/heartbeat-status` (frozen
   surface, SPEC §5), executed per-node with `consensusIdenticalAggregation`,
   then zod-validated in DON mode.
3. **Condition** — `selectFiring()`: `stale && critical` (plus optional
   `guardianAllowlist`). No firing guardian ⇒ log and return.
4. **Step 2 (optional)** — chain-read leg (`onchain.mode`: `off | observe |
   require`): reads the latest `Heartbeat` event for the guardian from the
   anchor contract and derives the on-chain heartbeat age. `observe` logs it;
   `require` gates the fire on corroborated staleness.
5. **Step 3** — POST `<breaker.baseUrl>/breaker/trigger` with the **frozen**
   body bytes `{"guardian":"0x…","reason":"…","requested_at_ms":n}` and header
   `X-Breaker-Signature: sha256=<HMAC_SHA256(secret, body)>` (constant-time
   verified by the breaker; SPEC §5 test vector is asserted by the runner's
   `--self-test`). `cacheSettings` deduplicates the non-idempotent POST across
   DON nodes.
6. **Step 4 (optional)** — Telegram `sendMessage` alert, best effort
   (`telegram.enabled`, default off).

The secret is read with `runtime.getSecret({id})`; in simulation the value
comes from the project `.env` via `../secrets.yaml`, in production from the
Vault DON (`cre secrets …`). The `requested_at_ms` timestamp uses
`runtime.now()` (DON time — deterministic across nodes).

## Secrets

`../secrets.yaml` maps logical IDs to environment variables:

```yaml
secretsNames:
  BREAKER_ARM_SECRET: [BREAKER_ARM_SECRET]
  TELEGRAM_BOT_TOKEN: [TELEGRAM_BOT_TOKEN]
```

For local runs copy `../.env.example` → `../.env` and set
`BREAKER_ARM_SECRET=demo-secret` (must match the breaker's value). Never
commit `.env`. For deployed workflows store the values in the Vault DON.

## Why JSON-RPC instead of the EVM capability (chain-read leg)

The CRE EVM read/write capabilities require the target chain to be registered
with the workflow's chain selector (and, per tenant, enabled — see
`cre workflow supported-chains`, which is login-gated). The Sentinel anchor on
a local anvil (or an arbitrary devnet) is **not** in the chain-selectors
registry, so the straightforward path was a direct JSON-RPC read
(`eth_blockNumber` / `eth_getLogs` / `eth_getBlockByNumber`) issued through
the CRE **HTTP capability**. That keeps the leg: (a) dependency-free,
(b) identical in simulation and in the fallback runner, and (c) testable
end-to-end against a real contract today. As a consequence `project.yaml`
deliberately configures no `rpcs:` — the EVM capability is never used. If a
future revision adopts the EVM capability (e.g. on Monad testnet, which the
CLI supports since v1.30.0), add an `rpcs:` entry and swap the leg for
`EVMClient.callContract`.

## Simulation status and fallback (SPEC §6)

`cre workflow simulate` **requires an authenticated CRE account** (“Run
`cre login` interactively, or set `CRE_API_KEY`”; this host has neither —
STUB-03). `cre workflow build` and `cre workflow hash` do run locally and
both succeed (see `../README.md` status table), which proves the folder is a
structure the installed CLI accepts and compiles.

Fallback (used for the evidence in `docs/evidence/p14-cre-simulate.txt`):

```bash
node tools/stale-breaker-emulator.ts  &     # injected stale state on :9091
BREAKER_ARM_SECRET=demo-secret node tools/local-runner.ts \
  --config config.local.json --expect fire   # same core.ts as the CRE handler
```

The runner prints a labelled `LOCAL SIMULATION (fallback) — NOT the CRE
runtime` banner; every decision, body byte, and signature it produces comes
from `src/core.ts`, the identical module the CRE handler calls.

## Checks

```bash
npx tsc --noEmit                     # workflow code (cre-sdk globals, types: [])
npx tsc -p tsconfig.tools.json --noEmit   # tools (Node types)
node tools/local-runner.ts --self-test    # frozen SPEC-P14 §5 HMAC vector
```

Notes: `typescript` is pinned to `5.9.3` because TS 7 breaks the CRE
compiler's validate step; `bun` must be on `PATH` for `cre workflow build`
(the CLI shells out for TS compilation). `@noble/hashes@2.2.0` matches the
version bundled by `@chainlink/cre-sdk@1.23.0` (single copy in the WASM
bundle); imports use the v2 `…/hmac.js` subpath style.
