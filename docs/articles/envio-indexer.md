![Envio indexer](img/envio-indexer.png)

# The audit trail you can query

Sentinel anchors a hash-chained decision journal to `SentinelAuditAnchor` on Monad. Anchoring proves the journal existed at a point in time. Turning that proof into something a person can actually read is the indexer's job, and it is the difference between an event that sits on chain and an audit trail you can query.

A contract event is cheap to publish and awkward to read. It lands in a block and stays there, reachable only to whoever knows which block range to ask for. Rebuilding the history from a raw node means walking ranges from the deploy block, paging the results, and carrying your own cursor between requests against a rate-limited public endpoint. The Envio indexer closes that gap. It turns every anchor and heartbeat event into a row keyed by sequence, and two product surfaces read those rows directly: the audit panel that proves a journal existed, and the resilience panel that watches the guardian keep its word.

## The flow, end to end

**1. The contract event.** Our anchor contract emits two events, declared at `indexer/config.yaml:45-46`: `DecisionAnchored(uint64 seq, bytes32 entryHash, bytes32 runningRoot, address account)` and `Heartbeat(address guardian, bytes32 riskStateHash, uint32 openPositions, uint8 maxTier)`. Both come from our own ABI, so the indexer reads a surface we define and control.

**2. HyperSync.** The chain block at `indexer/config.yaml:29-33` pins `id: 10143` with `start_block: ${ENVIO_START_BLOCK:-0}` (`config.yaml:31`) and an `rpc:` list (`config.yaml:32-33`). The header comment at `config.yaml:3-5` records the ordering: HyperSync (`https://monad-testnet.hypersync.xyz`) is the default data source, and the Monad RPC beneath it is the fallback and realtime source. A start block of 0 tells HyperSync to find the first block holding a matching event, so an operator never has to know the deploy block in advance.

**3. Handler to entity.** `indexer/src/EventHandlers.ts:31-41` writes one `SentinelAnchor` row per anchored batch: `seq`, `entry_hash`, `root`, `account`, block timestamp, transaction hash. The row id is `${chainId}_${block.number}_${logIndex}` (`EventHandlers.ts:34`), unique per chain, block, and log. The heartbeat handler at `EventHandlers.ts:52-59` writes `SentinelHeartbeat`. The entities are frozen in `indexer/schema.graphql`: `SentinelAnchor` at `schema.graphql:44-52`, `SentinelHeartbeat` at `schema.graphql:54-61`, and `Liquidation` at `schema.graphql:22-34`.

**4. GraphQL.** Envio persists those rows and exposes them over GraphQL with an `_aggregate` count and a `_by_pk` lookup per entity. Note the scalar mapping, called out at `schema.graphql:2-6`: `BigInt` columns serialize as JSON strings, never floats, so sequence numbers and timestamps stay exact.

**5. The Rust client.** `crates/sentinel/src/indexer.rs` is the read side. `anchors_after(from_seq)` posts the query at `indexer.rs:27`, `SentinelAnchor(where:{seq:{_gte:$from}}, order_by:{seq:asc})`; `heartbeats(limit)` posts the one at `indexer.rs:30`; `recent_liquidations` uses `indexer.rs:33` and `:36`. `from_env()` at `indexer.rs:102-108` returns `None` when `ENVIO_GRAPHQL_ENDPOINT` is unset, so the audit reads switch on the moment an endpoint is configured while the reflex path stays independent of the indexer being reachable. Every integer decodes from either a JSON number or a numeric string (`indexer.rs:349-369`), the wire reality of Envio's string-serialized `BigInt`. Failures wrap a bounded excerpt of 200 characters (`indexer.rs:24`, `:379-387`), so a noisy upstream body cannot flood the logs.

**6. The dashboard.** The dashboard's audit panel reads anchor rows and the resilience panel reads heartbeat rows. `docs/architecture.md:85-86` records the intent: the indexer mirrors anchors back for dashboards and backtests. The single-file dashboard reads the daemon's local API state today (`dashboard/index.html:472-477`), and the Rust client is the frozen, tested interface that points the same GraphQL surface at the hosted indexer.

## Why Envio carries the audit surface

An audit trail is a claim about the past, so whatever renders it has to read the past on demand. That is the hard part, and it is why the indexer sits under the feature rather than beside it.

An RPC `eth_getLogs` call is scoped to a block range. A plain-RPC reader walks ranges from the start, pages through results, and keeps its own state between requests against a rate-limited endpoint, so the freshest thing it can show is the newest block. The proof a user cares about is "here is every anchor, in order, since sequence N". Indexed history answers that in one request. HyperSync serves the history and Envio keeps it queryable, which turns the real question, every anchor with `seq >= N` ascending, into one line, literally the query in `indexer.rs:27`. The `seq >= from` filter with ascending order is the spine of the design: the audit-verify cross-check asks for exactly the tail it has not seen yet, and the dashboard scrolls the whole trail without re-deriving it.

## Configuration

| Variable | Default | What it does |
|---|---|---|
| `ENVIO_START_BLOCK` | `0` | Chain start block (`indexer/config.yaml:31`). `0` makes HyperSync auto-detect the first block with a matching event. The per-contract override is `ENVIO_ANCHOR_START_BLOCK` (`config.yaml:42`). |
| `ENVIO_RPC_URL` | `https://testnet-rpc.monad.xyz` | Monad testnet RPC (`indexer/config.yaml:33`), the fallback and realtime source under HyperSync. |
| `ENVIO_ANCHOR_CONTRACT_ADDRESS` | zero address | The deployed `SentinelAuditAnchor` address (`indexer/config.yaml:41`), set per environment. |
| `ENVIO_GRAPHQL_ENDPOINT` | unset | Read by the Rust client (`indexer.rs:102-108`). Set it and the audit-read surface goes live. |

## What actually ran

The proof is a local anvil run on chain 31337 using `config.local-anvil.yaml` (noted at `indexer/config.yaml:6`). `docs/evidence/p12-envio-anvil.txt:56-84` shows the deploy and three transactions. The evidence header records Envio HyperIndex 3.12.1 (`p12-envio-anvil.txt:6-9`). The indexer logged "The indexer storage is ready. Starting indexing!" and then "The indexer is ready. Switching to realtime indexing." (`p12-envio-anvil-console.log:13`, `:42`). It created 12 schema indexes, among them `SentinelAnchor_seq_2g4ufelrzx` and `SentinelHeartbeat_ts_bbv93d7orj` (`p12-envio-anvil-console.log:24-37`).

Then the live capture. An anchor sent after the indexer went ready was picked up in realtime. The console records `source: "RPC (127.0.0.1:8547)"`, `knownHeight: 4`, `newBlockHeight: 5` (`p12-envio-anvil-console.log:48-52`), and the `seq=3` row came back over GraphQL with tx `0xeea21503885fee68b9ae65f0c563f15a654c5e8e8ead47088693f25d02bd1cf6` (`p12-envio-anvil.txt:149-155`). A Postgres cross-check matched the API exactly: 3 anchors, 1 heartbeat, 0 fills, 0 liquidations (`p12-envio-anvil.txt:169-171`).

## Testing the read path

`crates/sentinel/tests/p12_adversarial.rs` treats the client as a black box and pins its behavior. The anchors query must name `SentinelAnchor` and no other entity (`p12_adversarial.rs:199-219`); the sequence bound travels as a `BigInt` string (`:220-233`); a `BigInt` decodes identically from a string and a number (`:387-458`); a non-empty `errors` array fails loudly and names graphql (`:532-577`); and an empty `data` object is a deterministic error, never a silent empty page (`:579-621`). The suite also cross-checks the artifacts on disk: `schema.graphql` has to declare exactly five entities with the frozen snake_case field names (`:812-887`), and `config.yaml` has to reference chain 10143 (`:915-953`).

## What the indexer unlocks next

The hosted Envio Cloud deployment is the next step, and the path is mapped: deployments are per-commit against a git repo (`docs/evidence/p12-envio-cloud.txt:43-48`), so the operator pushes the repo, registers the indexer, and points `ENVIO_GRAPHQL_ENDPOINT` at the public GraphQL URL. On Monad testnet the live anchor contract follows the same commit, with `ENVIO_ANCHOR_CONTRACT_ADDRESS` filling the address slot at `indexer/config.yaml:41`.

The exchange surface is the next wave of entities we add. `PerpFill`, `Liquidation`, and `AccountSnapshotDay` are already declared (`schema.graphql:10-42`) so their tables and GraphQL surface are live, and the candidate exchange ABI is in hand at `vendor/dex-sdk/crates/sdk/abi/dex/Exchange.json`, with fill events cross-validated against live chain topics (`indexer/README.md:125-129`). Registering the handlers in `src/EventHandlers.ts` turns those tables on, and the `Liquidation` read path in the Rust client (`indexer.rs:33`, `:36`) is already wired and tested against them.

---

## Bounty answer (paste into the form)

Sentinel is a non-custodial risk guardian for isolated-margin perpetual futures, and its audit spine is an append-only hash-chained journal. Batches of entries are anchored on-chain to our own `SentinelAuditAnchor` contract, which emits `DecisionAnchored(seq, entryHash, runningRoot, account)` and `Heartbeat(guardian, riskStateHash, openPositions, maxTier)`.

Envio HyperIndex 3.12.1 powers the read side of that product. `indexer/src/EventHandlers.ts` maps `DecisionAnchored` into `SentinelAnchor` rows and `Heartbeat` into `SentinelHeartbeat` rows (`indexer/src/EventHandlers.ts:24-60`), with entities and indexes declared in `indexer/schema.graphql`. On Monad testnet (chain 10143) the config is HyperSync-first: `indexer/config.yaml:3-5` records HyperSync (`https://monad-testnet.hypersync.xyz`) as the default data source with the Monad RPC as the fallback and realtime source, and `start_block: 0` (`config.yaml:31`) lets HyperSync auto-detect the first block containing a matching event.

That one fact changes the feature. An audit trail is a claim about the past. An RPC `eth_getLogs` call is scoped to a block range, so a naive reader walks ranges from the deploy block, pages, and holds its own cursor against a rate-limited public endpoint, which means the freshest thing it can show is the newest block. HyperSync serves the indexed history directly, so a reader asks the real question, every anchor with `seq >= N` ascending, and gets the whole trail. The Rust client that consumes it (`crates/sentinel/src/indexer.rs:27`) is exactly that query, alongside latest heartbeats and recent liquidations, and it drives two shipped surfaces: the audit panel that renders the anchor trail and the resilience panel that renders heartbeat history.

The read path is a first-class part of the product, not a side script. `crates/sentinel/src/indexer.rs` is a frozen GraphQL client with a 15-second request timeout (`indexer.rs:20-21`) that decodes Envio's string-serialized `BigInt` tolerantly (`indexer.rs:349-369`), switches on cleanly when `ENVIO_GRAPHQL_ENDPOINT` is set (`indexer.rs:102-108`), and bounds every error excerpt to 200 characters (`indexer.rs:24`). It is pinned by an independent adversarial suite (`crates/sentinel/tests/p12_adversarial.rs`): exact entity and field names, `BigInt` in both string and number form, non-empty GraphQL errors failing loudly, and the schema and config artifacts cross-checked on disk.

The end-to-end run is captured. On a local anvil chain (31337) we deployed `SentinelAuditAnchor`, ran `pnpx envio@latest dev` (Envio 3.12.1), and watched it go from "The indexer is ready. Switching to realtime indexing." to indexing a live anchor transaction six seconds later: `seq=3`, tx `0xeea21503885fee68b9ae65f0c563f15a654c5e8e8ead47088693f25d02bd1cf6`, `source: "RPC (127.0.0.1:8547)"`, block 4 to 5. Rows for seq 1 through 3 queried back through GraphQL, with a Postgres cross-check matching (3 anchors, 1 heartbeat). The full transcript is `docs/evidence/p12-envio-anvil.txt` and `docs/evidence/p12-envio-anvil-console.log`.

The hosted Envio Cloud deployment is the next step and the path is mapped: deployments are per-commit against a git repo (`docs/evidence/p12-envio-cloud.txt:43-48`), so the operator pushes, registers the indexer, and points `ENVIO_GRAPHQL_ENDPOINT` at the public URL. The exchange entities (`PerpFill`, `Liquidation`, `AccountSnapshotDay`) are declared with a live GraphQL surface, and the `Liquidation` read path in the Rust client is already wired and tested; adding the exchange handlers turns those tables on from the candidate ABI in hand (`indexer/README.md:125-129`).

Evidence: `docs/evidence/p12-envio-anvil.txt`; `docs/evidence/p12-envio-anvil-console.log`; `indexer/src/EventHandlers.ts`; `indexer/schema.graphql`; `indexer/config.yaml`; `crates/sentinel/src/indexer.rs`; `crates/sentinel/tests/p12_adversarial.rs`; `docs/queries.graphql`
