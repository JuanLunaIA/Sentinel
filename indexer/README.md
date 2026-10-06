# sentinel-indexer — Envio HyperIndex

Envio HyperIndex (v3) project for **Sentinel** (P12). It indexes the on-chain
audit trail emitted by our own `SentinelAuditAnchor` contract on **Monad** and
exposes it as a GraphQL API.

| | |
|---|---|
| Project name | `sentinel-indexer` |
| Primary network | Monad **testnet**, chain `10143` (HyperSync-backed; mainnet `143` is a one-line swap) |
| Contract | `SentinelAuditAnchor` (see `../contracts/src/SentinelAuditAnchor.sol`) |
| Events | `DecisionAnchored(uint64,bytes32,bytes32,address)` → `SentinelAnchor`; `Heartbeat(address,bytes32,uint32,uint8)` → `SentinelHeartbeat` |
| Envio CLI | `envio@3.12.1` (latest at build time; installed as a project dependency) |

## Layout

```
config.yaml               Monad testnet config (production target)
config.local-anvil.yaml   Local anvil (chain 31337) config for the e2e test
schema.graphql            FROZEN entities: PerpFill, Liquidation, AccountSnapshotDay,
                          SentinelAnchor, SentinelHeartbeat (names verbatim per spec)
src/EventHandlers.ts      Anchor/heartbeat handlers (our ABI)
src/indexer.test.ts       Vitest handler tests (simulated events, offline)
.env.example              Documented environment variables
```

## Entities → events

| Entity | Source | Status |
|---|---|---|
| `SentinelAnchor` | `SentinelAuditAnchor.DecisionAnchored` | ✅ indexed |
| `SentinelHeartbeat` | `SentinelAuditAnchor.Heartbeat` | ✅ indexed |
| `PerpFill`, `Liquidation`, `AccountSnapshotDay` | Perpl exchange (testnet `0x1964c32f0be608e7d29302aff5e61268e72080cc`) | ⏳ tables + GraphQL surface exist; handlers pending — see roadmap below |

## Environment variables

Only `ENVIO_`-prefixed variables reach the hosted service, so those are the
canonical names; the repo-standard bare name is accepted as a secondary
fallback in `config.yaml`.

| Variable | Used by | Meaning |
|---|---|---|
| `ENVIO_API_TOKEN` | HyperSync / Envio Cloud | token from https://envio.dev/app/api-tokens |
| `ENVIO_ANCHOR_CONTRACT_ADDRESS` (or `ANCHOR_CONTRACT_ADDRESS`) | config.yaml | deployed `SentinelAuditAnchor` address (STUB-17: set after the live deploy; zero-address placeholder until then) |
| `ENVIO_START_BLOCK` / `ENVIO_ANCHOR_START_BLOCK` | config.yaml | start blocks; `0` = HyperSync auto-detects the first event block |
| `ENVIO_RPC_URL` | config.yaml | Monad testnet RPC (fallback/realtime source; default `https://testnet-rpc.monad.xyz`) |
| `ENVIO_LOCAL_RPC_URL` | config.local-anvil.yaml | local anvil RPC (default `http://127.0.0.1:8547`) |
| `HASURA_EXTERNAL_PORT` | local dev stack | host port for the local Hasura GraphQL endpoint (default `8080`; this repo's `.env` uses `8888` because the host already serves a service on 8080) |

## Commands

```bash
pnpm install          # deps + codegen-friendly setup
pnpm codegen          # regenerate .envio/types from config.yaml + schema.graphql
pnpm exec tsc --noEmit
pnpm test             # vitest: handler tests with simulated events (offline)
pnpm dev              # local dev: Docker (Postgres + Hasura) + indexer, Monad testnet
```

GraphQL (local dev): `http://localhost:8888/v1/graphql`, admin secret `testing`.
See `../docs/queries.graphql` for the four example queries.

## Local anvil end-to-end (proof runbook)

Proven at P12 — full console + query output in
`../docs/evidence/p12-envio-anvil.txt` (raw console:
`p12-envio-anvil-console.log`).

```bash
anvil --silent --port 8547 &
cd ../contracts && forge build
forge create src/SentinelAuditAnchor.sol:SentinelAuditAnchor \
  --rpc-url http://127.0.0.1:8547 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --broadcast

cast send <addr> 'anchor(uint64,bytes32,bytes32)' 1 0x11..32 0x22..32 \
  --rpc-url http://127.0.0.1:8547 --private-key 0xac09..
cast send <addr> 'beat(bytes32,uint32,uint8)' 0x55..32 3 2 \
  --rpc-url http://127.0.0.1:8547 --private-key 0xac09..

cd ../indexer
DOCKER_HOST=... HASURA_EXTERNAL_PORT=8888 pnpx envio@latest dev --config config.local-anvil.yaml
curl -s http://127.0.0.1:8888/v1/graphql \
  -H 'x-hasura-admin-secret: testing' -H 'content-type: application/json' \
  -d '{"query":"{ SentinelAnchor(order_by: {seq: asc}) { seq entry_hash root ts tx_hash } }"}'
```

Notes discovered during the P12 run (see the evidence file for exact detail):

- **Host firewall**: on this machine a system `nftables` `inet filter` table has
  `forward` policy `drop`, so Docker containers cannot reach each other
  (Postgres ↔ Hasura). The local stack needs a narrow, temporary rule:
  `sudo nft add rule inet filter forward iifname "<envio-bridge>" oifname "<envio-bridge>" accept`
  (delete it again afterwards; the exact rule + handle are recorded in the
  evidence file).
- **Docker access**: if the session user cannot access `/var/run/docker.sock`,
  either re-login (docker group) or run a scoped socket relay
  (`sudo socat UNIX-LISTEN:<path>,mode=666,fork UNIX-CONNECT:/var/run/docker.sock`)
  and set `DOCKER_HOST=unix://<path>`.
- **pnpm 11**: esbuild's build script must be approved (`pnpm approve-builds
  esbuild`) — recorded in `pnpm-workspace.yaml` (`allowBuilds`).

## Perpl-event indexing: roadmap (ABI unresolved, fallback applied)

The P12 spec allowed the Perpl **exchange** contract
(`0x1964c32f0be608e7d29302aff5e61268e72080cc`, Monad testnet) to be included
only if its ABI could be resolved via the testnet explorer within the 45-minute
budget. That budget was exhausted with the ABI **unresolved** (full attempt log:
`../docs/evidence/p12-envio-perpl-abi-attempts.txt`), so the sanctioned fallback
is in place:

- the exchange contract stays **configured-but-commented** in `config.yaml`
  (contract `(b)` block) with its address and testnet start block (`62953`),
- the `PerpFill` / `Liquidation` / `AccountSnapshotDay` entities exist in the
  schema (tables + GraphQL surface are live and queryable — they simply return
  empty), and
- no exchange handlers are registered in `src/EventHandlers.ts`.

Why the explorer failed: `testnet.monadexplorer.com/api` 308-redirects to
`testnet.monadvision.com`, which is behind a Cloudflare bot challenge (403
"Just a moment…", also in the browser); `api.monadscan.com` is a deprecated V1
endpoint pointing at Etherscan API V2, which requires an API key we don't have;
the contract is not on Sourcify.

What the next wave already has in hand:

1. **A candidate ABI**: `../vendor/dex-sdk/crates/sdk/abi/dex/Exchange.json`
   (perpl-sdk) contains the full exchange ABI — 204 events. Two fill events
   cross-validate against live chain topics: in the 100-block window
   `68581994–68582094` of Monad testnet the contract emitted
   `MakerOrderFilledV2` ×21 and `TakerOrderFilledV2` ×20 (topic0s computed from
   the vendor ABI signatures appear verbatim in `eth_getLogs`).
2. **The mapping caveats to resolve before enabling**: the events carry
   numeric exchange ids (`perpId`, `accountId`, `posAccountId`) — the frozen
   `account String` fields need the account-id → address relation (see the
   exchange's `AccountCreated(address,uint256)`), and `TakerOrderFilledV2`
   carries no id fields at all, so a naive per-event mapping would be wrong.
   Scale/decimals (`*PNS`/`*LNS`/`*CNS`) must be decoded into the decimal
   strings the schema expects.
3. **Wiring**: copy the ABI into `indexer/abis/`, uncomment the `(b)` block in
   `config.yaml` (fill in the two event signatures), add handlers for the
   three entities, then re-run codegen + the e2e runbook.
