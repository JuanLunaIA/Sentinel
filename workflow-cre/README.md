# workflow-cre — Sentinel dead-man's-switch (CRE project)

Chainlink CRE (Chainlink Runtime Environment) project for **SPEC-P14 §6**: a
cron-triggered workflow that watches the Sentinel breaker's heartbeat status
and, when a guardian is `stale && critical`, POSTs the frozen signed
`/breaker/trigger` request (plus an optional Telegram alert).

```
workflow-cre/
├── project.yaml              # CRE project settings (targets; no rpcs needed — see note)
├── secrets.yaml              # logical secret names -> env vars (simulation)
├── .env.example              # local secret values (copy to .env; never commit)
└── sentinel-deadman/         # the workflow folder (workflow.yaml + main.ts + src/)
```

## Status (verified 2026-10-06, CLI v1.37.0)

| Command | Result |
|---|---|
| `cre workflow build sentinel-deadman --target local-simulation` | **exit 0** — compiles to WASM (2.4 MB binary, hash `904b5f68…` reported) |
| `cre workflow hash sentinel-deadman … --public_key 0xf39F…2266` | **exit 0** — workflow hash `00cb333e…` printed |
| `cre workflow simulate sentinel-deadman --non-interactive --trigger-index 0 …` | **exit 1 — Authentication required** (no CRE account/login on this host; tracked as STUB-03 / PENDING-ACCOUNT, see `docs/evidence/p14-cre-simulate.txt`) |
| `cre workflow supported-chains` | **exit 1 — Authentication required** (per-tenant chain list needs login; `docs/evidence/p14-cre-cli-probe.txt`) |

Two toolchain requirements were discovered while getting `cre workflow build`
to pass (both recorded in the evidence):

1. **bun must be on `PATH`** (`~/.bun/bin/bun`, installed from bun.sh —
   user-local, no sudo). Without it the CLI fails with
   "bun is required for TypeScript workflows".
2. **TypeScript must be 5.x** (`typescript@5.9.3`). TypeScript 7 (the native
   rewrite) breaks the CRE compiler's validation step
   (`ts.ScriptTarget` undefined in `cre-sdk/scripts/.../validate-shared.ts`).

Because `simulate` requires a logged-in CRE account, the SPEC §6 fallback is
implemented and used in this environment: **`sentinel-deadman/tools/local-runner.ts`**
executes the *same* definition (the shared `sentinel-deadman/src/core.ts`
module the CRE handler calls) against the emulated breaker endpoint
`sentinel-deadman/tools/stale-breaker-emulator.ts`. See the workflow README
and `docs/evidence/p14-cre-simulate.txt` for the full transcript.

## Running the simulation (once an account exists)

```bash
cd workflow-cre
cp .env.example .env                 # BREAKER_ARM_SECRET=demo-secret
export PATH="$HOME/.bun/bin:$PATH"   # cre workflow build/compile needs bun
# terminal A: a breaker (or the emulated stale endpoint)
node sentinel-deadman/tools/stale-breaker-emulator.ts
# terminal B:
cre workflow simulate sentinel-deadman --non-interactive --trigger-index 0 --target local-simulation
```

`cre login` (browser flow) or `CRE_API_KEY` is required first; the per-tenant
chain allowlist for the eventual deploy comes from `cre workflow supported-chains`.

## Fallback runner (this environment)

```bash
cd workflow-cre/sentinel-deadman
npm install
node tools/stale-breaker-emulator.ts &                                   # stale endpoint on :9091
BREAKER_ARM_SECRET=demo-secret node tools/local-runner.ts --config config.local.json --expect fire
node tools/local-runner.ts --self-test        # frozen SPEC-P14 §5 HMAC vector
```

## The on-chain leg

`config.local.json` points the optional chain-read leg at
`http://127.0.0.1:8547` (local anvil) and `config.production.json` at
`https://testnet-rpc.monad.xyz`. The leg reads the SentinelAuditAnchor
`Heartbeat` events over **plain JSON-RPC via the CRE HTTP capability** — see
`sentinel-deadman/README.md` ("Why JSON-RPC instead of the EVM capability")
for the rationale, including why `project.yaml` intentionally carries no
`rpcs:` list.

## Spec / links

- `SPEC-P14.md` §6 (frozen), docs.chain.link/cre (CLI, HTTP client, cron
  trigger, secrets-in-simulation), `docs/FACTS.md` §5 (CRE facts, Monad support).
