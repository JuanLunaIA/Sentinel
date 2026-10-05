# FACTS.md — Verified Ground Truth (P01)

**Verification date:** 2026-10-05 (UTC) · **Method:** live probes + vendored official repos at pinned commits + on-chain reads.
Every claim below maps to a file in `docs/evidence/` (see §0.9 index) or a cited file in `vendor/` (gitignored clones, re-created by `scripts/verify-facts.sh`).

**Vendor provenance (pinned commits):**
- `vendor/api-docs` @ `25ab6e2c75c8f84d0550da8c3be30af49ad631f2` (2026-09-25)
- `vendor/dex-sdk` @ `01b9910761755b0a0d9c710c1ede62ab937daa7d` (2026-09-23)
- `vendor/dex-sdk-examples` @ `c2a30986ac5f323faf3db30d6a8b59972647ca66` (2026-01-13)
→ `docs/evidence/p01-vendor-provenance.txt`

---

## 0. Discrepancies vs the P00 FACT SHEET (read first)

| # | P00 FACT SHEET said | Verified reality | Impact / action |
|---|---------------------|------------------|-----------------|
| 0.1 | Testnet collateral `0xdf5b718d8fcc173335185a2a1513ee8151e3c027` | **LIVE testnet collateral = `0xa9012a055bd4e0edff8ce09f960291c09d5322dc`** (symbol `AUSD`, 6 dec, confirmed via `/v1/pub/context` and `cast call`). The `0xdf5b…` contract also exists and reports `AUSD` (older instance); `api-docs/.env.example` is stale. | Use `0xa901…22dc` everywhere. Evidence: `p01-perpl-context-testnet.json`, `p01-cre-cast-checks.txt` |
| 0.2 | Rust pin `1.85.0` (P02 spec) | **Impossible**: `alloy 2.5.0` chain (perpl-sdk dep) requires rustc ≥ **1.94.1**; dex-sdk pins `1.97`. | Pin project toolchain to **1.97**. Check @1.85 fails (`p01-sdk-rust185-fail.log`), check @1.97 **passes** in 5m14s (`p01-sdk-rust197-check.log`) |
| 0.3 | (env example) SOL mainnet id `30` | **SOL mainnet = 31** (live + official docs). Fact sheet was right; the shipped `.env.example` is stale. | Trust live context |
| 0.4 | CRE CLI `npm i -g @chainlink/cre-cli` | npm package does **not** exist (404). Real install = **GitHub releases** (`smartcontractkit/cre-cli`); installed **v1.37.0**, sha256 verified vs official `checksums.txt` (`1e660e95…536503`). Version cmd: `cre version`. | Use installed binary `~/.local/bin/cre`. Evidence: `p01-cre-cli-download.txt` |
| 0.5 | Markets list (6 mainnet / 5 testnet) | Live has more: **mainnet** 1 BTC, 10 MON, 20 ETH, 31 SOL, 40 HYPE, 50 ZEC, **60 LIT, 70 VVV, 90 PUMP, 100 NEAR, 110 UNI**; **testnet** 16 BTC, 32 ETH, 48 SOL, 64 MON, 256 ZEC, **272 LIT, 320 PUMP, 336 NEAR** | Read market set dynamically from `/v1/pub/context`; never hardcode |
| 0.6 | — | `min_account_open_amount`: **mainnet `10000000` (10 AUSD)**, **testnet `100000000` (100 AUSD)** (6 dec) | Needed for SETUP-MANUAL + policy |
| 0.7 | "fetch smart contract info" as separate surface | It is derived from `GET /v1/pub/context` `instances[0]` (see `vendor/api-docs/examples/rust/src/bin/fetch_smart_contract_info.rs`) | No extra endpoint to build |
| 0.8 | — | Testnet exchange **instance id = 12** (mainnet = 1); deploy blocks: mainnet `54773010`, testnet `62953` (docs) | Use for log scans/indexer start block |

---

## 1. Perpl — perp DEX on Monad (isolated margin)

### 1.1 Networks & contracts  (`p01-perpl-context-{mainnet,testnet}.json`, `p01-connectivity.txt`)

| Field | Mainnet | Testnet |
|---|---|---|
| REST base | `https://app.perpl.xyz/api` | `https://testnet.perpl.xyz/api` |
| WebSocket base | `wss://app.perpl.xyz` | `wss://testnet.perpl.xyz` |
| Chain ID | `143` | `10143` |
| RPC | `https://rpc.monad.xyz` | `https://testnet-rpc.monad.xyz` |
| Exchange contract (UUPS proxy, code verified) | `0x34B6552d57a35a1D042CcAe1951BD1C370112a6F` | `0x1964c32f0be608e7d29302aff5e61268e72080cc` |
| Collateral token (symbol verified on-chain) | AUSD `0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a` | AUSD `0xa9012a055bd4e0edff8ce09f960291c09d5322dc` (see 0.1) |
| Explorer | https://monadscan.com | https://testnet.monadexplorer.com |
| Exchange deploy block | `54773010` | `62953` |

### 1.2 Markets (live `/v1/pub/context`)

Full id table in §0.5. Quirk: BTC (1) and MON (10) on mainnet carry an **empty `symbol`** field (use `name` or id). Per-market config example (ETH both networks):

- `funding_interval_sec=2580`, `order_ttl_blocks=20`, `order_retry_blocks=22`, `price_decimals=2`, `size_decimals=3`, `contract_version=[1,7,5]`
- `order_max_market_slippage_bps`: mainnet 100 / testnet 1000; `order_max_neg_pnl_collat_bps`: mainnet 300 / testnet 1000
- `initial_margin=1200`, `maintenance_margin=2000` → see §1.7 for semantics (12x max leverage; 5% maintenance margin)
- Fee schedule (base tier micros): maker `45`, taker `345`; tier arrays `maker_fees=[45,25,15,0,…]`, `taker_fees=[345,300,250,210,175,150,125,0]` (mainnet ETH; testnet ETH identical base). Read per-market at runtime; never hardcode (`p01-perpl-market-extract.txt`).

### 1.3 REST surface

**Public (no auth):** `GET /v1/pub/context`; `GET /v1/market-data/:id/candles/:res/:from-:to` (max 1024); `GET /v1/market-data/:id/funding/:from-:to`; `GET /v1/market-data/funding/:from-:to`; `GET /v1/market-data/:id/book?levels=1..100` (mt:15 shape); `GET /v1/market-data/:id/ticker`; `GET /v1/market-data/ticker` (mt:9 shape, keyed by market id); `GET /v1/profile/announcements`.

**Authenticated (X-API-* headers, read scope OK):** `GET /v1/trading/wallet` (mt:19 `Wallet`, carries `as[]` accounts with `fw`, `ft`, `lfr`, `b`, `lb`, and `sn` = block; **404 if no exchange account**); `GET /v1/trading/positions` (mt:26); `GET /v1/trading/orders` (mt:23); `GET /v1/trading/account-history` (event types: 1 Deposit, 3 IncreasePositionCollateral, 4 Settlement, 5 **Liquidation**, 8 Funding, 9 Deleveraging, 10 Unwinding, 11 PositionCollateralDecreased); `GET /v1/trading/fills`; `GET /v1/trading/order-history`; `GET /v1/trading/position-history`; `GET /v1/trading/portfolio/:kind(equity|pnl)/:period`; `GET /v1/profile/ref-code`.

**Order submission:** `POST /v1/trading/orders` — batch of 1–100 `OrderSpec` (`d[]`, optional `mt:30`, `sn` echoed as `cid`), **trade scope**, requires `fw=true`. Response `mt:31`: `status` (batch) + `statuses[]` per order **by position**. HTTP 200 = "judged", not accepted; code 0 = accepted for forwarding only. Rate limit: 1 unit per order. (Source: `vendor/api-docs/rest-endpoints.md`; walkthrough `vendor/api-docs/examples/rust/src/bin/submit_orders.rs`.)

### 1.4 Authentication (verified against `vendor/api-docs/authentication.md` + `.../examples/rust/src/lib.rs`)

- Ed25519 API key; scopes `1=read`, `2=trade` (implies read), `3=both`. Keys: max 16/profile; revoked keys can't re-enroll.
- **REST canonical string** (6 fields joined `\n`): `chain_id`, `HTTP_METHOD`, `request-target (path?query exactly as sent)`, `timestamp_ms`, `nonce (16B base64url, no pad)`, `sha256body hex`. Signature = `base64url(ed25519)` in `X-API-Signature`; headers `X-API-Key/Timestamp/Nonce/Signature`. Validity: timestamp within **30 s**; nonce single-use.
- **WS sign-in canonical** (4 fields): `chain_id`, `trading-ws-signin`, `timestamp_ms`, `nonce` → frame `mt:29 {chain_id, api_key, timestamp, nonce, signature}` sent as FIRST message; idle window 5 s mainnet / 10 s testnet.
- Rust reference implementation to mirror: `vendor/api-docs/examples/rust/src/lib.rs` (`signed_request_headers`, `ws_signin_frame`) — P03 unit-tests against these vectors.
- Enrollment: UI (`{app,testnet}.perpl.xyz/apikeys`) recommended; programmatic = EIP-712 wallet signature + Ed25519 proof-of-possession + whitelisted `Origin` (`POST /v1/api-key/payload` → `/v1/api-key/enroll`). Errors: 401 stale/bad sig · 403 scope · 404 no account.

### 1.5 Prerequisites for API trading (the 404 / sr:34 class of bugs)

Three separate things: API auth ≠ exchange account ≠ order forwarding.
- **Exchange account**: on-chain `createAccount(uint256)` with ≥ min open amount; UI "Deposit to Enable trading". Check: `cast call $EXCHANGE "getAccountByAddr(address)(uint256)" $WALLET`.
- **Order forwarding** (`fw`): off on new accounts; enable via `allowOrderForwarding(bool)` from the account's wallet (UI: **One-Click Trading** toggle). No on-chain getter — read `fw` from `mt:19`/`mt:21`. Missing → order acked `code 0` then fails `mt:24 st:7 sr:34` (no tx, no order id).
- Failure signatures: 404 on authenticated calls = no SCA; `sr:34` = forwarding disabled. (Source: `vendor/api-docs/README.md` §API Auth vs Smart Contract Account, §Enabling Order Forwarding.)

### 1.6 Order placement & reduce-only (P05 core)

`OrderSpec`: `rq, mkt, acc, oid?, t, p?, s, a?, ms?, mnp?, tif?, fl, tp?, tpc?, tr?, lp?, lv, lb, bf?` (exact shapes: `vendor/api-docs/types.md` §OrderSpec; WS doc §Placing Orders).
- **Reduce-only = `CloseLong (t=3)` / `CloseShort (t=4)`** — reduce-only by construction, clamped to position size.
- `rq`: strictly increasing per account; seed from `account.lfr`; `rq = max(counter, lfr) + 1`; **same-rq re-send = the retry/idempotency mechanism** (at-most-once); `rq <= lfr` → `sr:32`.
- `lb`: `0` (server default) or `head < lb <= head + market.order_ttl_blocks`; head from heartbeat `h` (WS) or `sn` of any trading-state/ticker response (HTTP).
- Transports: WS `mt:22` (one at a time) or HTTP batch (`mt:30/31`). Flow: `mt:3` ack (sid:100, `cid` echoes your frame `sn`) → outcome on `mt:24`; close code `1011` = frame-level failure, no `mt:3`.
- Post-fill truth: `mt:27`/positions re-read; `mt:25` fills (`l`: 1 maker, 2 taker; `f` negative = rebate).

### 1.7 Positions, margins & liquidation math (P04 core)

**Gateway `Position` has NO liquidation-price field.** Fields: `mkt, acc, pid, st, sr, sd (1 long/2 short), c (collateral, 6dec), ep (entry), s (size), fee, cfee, efs (entry funding sum), lv, dpnl, fnd, xp, ots…` (`vendor/api-docs/types.md` §Position). Liq price must be **derived**:

- Units: `initial_margin` / `maintenance_margin` are **leverage in hundredths** (`LEVERAGE_SCALE=2`, `num::Converter` divides by 100). `leverage_x = value/100`; **margin fraction = 100/value**. ETH: `1200 → 12x → 8.33% IM`; `2000 → 20x → 5% MMR`. (Sources: `vendor/dex-sdk/crates/sdk/src/state/perpetual.rs` L8, `.../num.rs`; `vendor/api-docs/types.md` comments "1000 = 10% (10x max)" / "2000 = 5%".)
- Reference formulas (exact SDK source `vendor/dex-sdk/crates/sdk/src/state/position.rs` L134-153):
  - `MMR = entry_price * size / (maintenance_margin/100)`
  - `liq_price = entry + side * (MMR − deposit − premium_pnl) / size`, `side = +1 long / −1 short`
  - `bankruptcy_price = entry − side * (deposit + premium_pnl) / size`
- Unit vectors for P04 tests (SDK tests, same file): entry 100, size 10, deposit 100, mm 20 → MMR 50; long liq **95**, short **105**; long bankruptcy **90**, short **110**; +50 premium → long liq 90 / short liq 110.
- `premium_pnl` accrues from funding (two `funding@` messages per interval, keyed by `feb`); gateway `efs` + live funding sums allow reconstruction (STUB-06: optional precision).

### 1.8 Fees

- Gateway fees are **micros (1e-6)**: `fee = notional * micros / 1e6`; effective rate = `tiers[account.ft] ?? base` (arrays are omitempty; range-check `ft`). ETH base: maker 45 (0.0045%), taker 345 (0.0345%).
- On-chain SDK scale differs: `Per100K` (1 = 0.1 bps) or ppm after contract v1.1.7.5 — discriminated by `ContractFeatures::fee_rate_converter` (`vendor/dex-sdk/crates/sdk/src/num.rs`). Do not mix gateway-micros with on-chain units.
- **Liquidations, deleveraging and unwinds pay no trading fee**; a close pays from its own proceeds. `f` is gross (protocol + `bfa` builder part).

### 1.9 Rate limits & close codes

- Trading WS: 60 req/min testnet, 120 mainnet; **4 connections per wallet (shared across keys + browser)**. Market-data WS: 10 req/min, 16 subscriptions, same both nets. REST: edge-limited (429 + backoff). Close codes: `1008` rate/idle (5 s mainnet / 10 s testnet), `1011` unparseable/unknown market (closes, no mt:3), `1013` back-pressure, `1001` restart, `3401` auth failure; `1006` = network loss. Source: `vendor/api-docs/README.md`, `websocket.md`.

### 1.10 Real-time streams

- Market-data streams: `heartbeat@<chain>` (head block `h`), `market-state@<chain>` (**mark price `mrk`** — the risk input), `candles@<id>*<res>`, `order-book@<id>` (mt:15 snap, mt:16 updates, `o:0` removes), `trades@<id>`, `funding@<chain>`, `market-config@<chain>`.
- Trading WS after `mt:29`: snapshots `mt:19` Wallet, `mt:23` Orders, `mt:26` Positions; updates `mt:20/21/24/25/27/28`; heartbeat `mt:100` with `sn` continuity seeded from WalletSnapshot `sn` (gap → reconnect). `mt:21` AccountUpdate is where `fw`, `ft`, `lfr`, `b` change.

### 1.11 Rust SDK assessment (decision)

- `perpl-sdk` (v0.2.9 workspace, `vendor/dex-sdk`) covers **on-chain state + direct on-chain execution**: exchange state cache (perpetuals, L3 book, accounts, positions), raw event stream (`stream::raw`) + normalized trades, order building/quantization (`types::OrderRequest::builder/build`), simulate/send via alloy (`exec::Call`, `exec::orders_call`), cancel/change. It does **not** cover the API-gateway Ed25519 auth, REST or WS — that's ours.
- **Decision:** implement the gateway client raw (P03), mirroring `api-docs/examples/rust`; use `perpl-sdk` as a **complement** (state cross-checks, on-chain fallback, bounty narrative "built on perpl-sdk"), path dep `perpl-sdk = { path = "vendor/dex-sdk/crates/sdk", default-features = false, features = ["display"] }` (avoids Anvil bindings). Toolchain **1.97** (§0.2). SDK local `testing` feature needs a custom Monad Anvil fork — optional (STUB-07).
- Market-id note: SDK `Chain::mainnet()` lists perpetuals [1,10,20,31,40,50] / testnet [16,32,48,64,256] — live exchange has more (§0.5); take markets from the gateway context.

---

## 2. Nansen — x402 v2 (VERIFIED live)

- Unpaid `POST /api/v1/smart-money/netflow` → **HTTP 402**, `x402Version: 2`, with **8 payment rails** — all four target endpoints verified (netflow, holdings, perp-leaderboard, profiler/perp-positions): `p01-nansen-402*.json`, `p01-nansen-endpoints.txt`.
- **Monad rail (our target):** `network: eip155:143`, `asset: 0x754704Bc059F8C67012fEd69BC8A327a5aafb603` (USDC), `amount: 50000` (6 dec = $0.05), `payTo: 0x93053f1e7A5eFEDa532Fe69CbbE43cBEc3A0F13f`, `scheme: exact`, `maxTimeoutSeconds: 300`. Other rails: Base (8453), XLayer (196), BSC (56) ×4 stable variants, Solana.
- Headers: pay + retry with **`PAYMENT-SIGNATURE`** (V2; legacy `X-PAYMENT` rejected); send **`X-Payer-Address`** on the initial unpaid request (promo "50% off first 100 settled calls" **not advertised** in `/.well-known/x402` at verification time — `p01-nansen-wellknown.body`; STUB-04).
- `.well-known/x402` also advertises an alternate `mpp` (Tempo) protocol — ignore, we build x402 v2.
- Body shape confirmed: `{"chains":["ethereum"]}`; exact schemas per endpoint confirmed at P09 (STUB-05). Cache 5 min; budget cap (P02: 40/h).
- **Plan B:** fixtures + local sidecar; never let a 402 stall the reflex path.

## 3. Alibaba Qwen — UNVERIFIED-PENDING-KEY (STUB-01)

Both `https://dashscope.aliyuncs.com/compatible-mode/v1` and `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` reachable (HTTP 401 unauthenticated, `p01-connectivity.txt`). Model `qwen3.8-max` per fact sheet; first keyed call in P07 confirms base + model + `response_format: json_object` + thinking-trace shape. Budget `max_tokens >= 4000`.

## 4. Kimi (Moonshot) — UNVERIFIED-PENDING-KEY (STUB-02)

`https://api.moonshot.ai/v1` reachable (401 unauthenticated). Exact model string (expected `kimi-k3` family) confirmed at P08 first keyed call. `.cn` base as fallback.

## 5. Chainlink CRE — VERIFIED, INCLUDING MONAD (upgraded from "unverified")

- **Monad support confirmed** in official docs: mainnet requires CRE CLI **≥ v1.29.0**; testnet **≥ v1.30.0** (`supported-networks-ts.mdx`, captured in `p01-cre-cast-checks.txt`). Installed CLI **v1.37.0** (`~/.local/bin/cre`, sha256 verified). Per-tenant enablement check at P14 (`cre workflow supported-chains` after login) — STUB-03.
- Install method (docs): GitHub releases `smartcontractkit/cre-cli` (npm name from P00 does not exist). `install.sh` also shipped; we did manual verified download.
- Workflows in Go or TS; triggers (cron/HTTP/EVM-log), capabilities incl. EVM read/write, HTTP client; `cre workflow simulate` for local simulation (target `local-simulation`), then broadcast. Source: docs.chain.link/cre (structure captured).

## 6. Envio HyperIndex — VERIFIED

- CLI: `pnpx envio@latest` → **envio 3.12.1** (`p01-connectivity.txt`/`p01-batch4`); `pnpx envio init` guided, `init contract-import` for contract-based scaffolding; language TS or ReScript; API token from https://envio.dev/app/api-tokens.
- **Monad first-class support both networks**: `monad` (143) and `monad-testnet` (10143); HyperSync endpoints `https://monad.hypersync.xyz` / `https://monad-testnet.hypersync.xyz`; Envio Cloud hosting free tier. Start blocks: mainnet `54773010`, testnet `62953` (exchange deploy blocks).

## 7. Monad infrastructure

- chainIds: mainnet `0x8f` (143), testnet `0x279f` (10143) — verified via RPC. Gas price (both): `0x17bfac7c00` = **102 gwei**.
- Faucet: **https://faucet.monad.xyz** (address + socials; also https://www.alchemy.com/faucets/monad-testnet 1 MON/24 h). Add-network values: chain `10143`, RPC `https://testnet-rpc.monad.xyz`, explorer `https://testnet.monadexplorer.com`.
- Explorers: mainnet monadscan.com (also monadvision.com), testnet testnet.monadexplorer.com.

## 8. Toolchain status (for P02)

| Need | Status |
|---|---|
| rustc/cargo | 1.98.1 installed; **1.85.0 + 1.97 installed**; project pins **1.97** (§0.2) |
| Foundry (cast/forge/anvil) | **installed** v1.8.5 (`~/.foundry/bin`, symlinked into `~/.local/bin`) — `p01-foundry-install.log` |
| Node / pnpm | node v26.7.0, pnpm 11.28.4 |
| CRE CLI | **v1.37.0** installed (`~/.local/bin/cre`) |
| Docker | engine 29.8.1, daemon up; session needs `sudo` or re-login for group (STUB-08); compose v5.5.1; buildx not installed (plain `docker build` OK) |

## 9. Evidence index (`docs/evidence/`)

Connectivity/toolchain: `p01-connectivity.txt`, `p01-recon-toolchain.txt`, `p01-recon-workspace.txt`.
Perpl: `p01-perpl-context-mainnet.json`, `p01-perpl-context-testnet.json`, `p01-perpl-market-extract.txt`, `p01-cre-cast-checks.txt` (cast symbol checks).
Nansen: `p01-nansen-402.json`, `p01-nansen-402.headers`, `p01-nansen-402-payer.json`, `p01-nansen-wellknown.body`, `p01-nansen-endpoints.txt`, `p01-nansen-402-*.json`.
SDK: `p01-sdk-coverage.txt`, `p01-sdk-rust185-fail.log`, `p01-sdk-rust197-check.log`, `p01-dex-sdk-clone.txt`, `p01-github-refs.txt`, `p01-vendor-provenance.txt`.
Tooling: `p01-cre-cli-download.txt`, `p01-cre-releases.txt`, `p01-foundry-install.log`.
Re-verify everything: `bash scripts/verify-facts.sh` → `p01-verify-latest.txt`.

## 10. Open items

See `docs/STUBS.md` (STUB-01..08) and `docs/SETUP-MANUAL.md` (human steps: testnet onboarding, API keys, wallets, bot).
