# SENTINEL — MANUAL SETUP CHECKLIST (final: keys -> live)

Machine-verifiable facts live in `docs/FACTS.md`; operations in `docs/RUNBOOK.md`.
**Rules:** secrets go into `.env` only — never into chat, commits, or this file.
Refreshed 2026-10-06 (key-readiness audit): every env name below is exactly what the
code reads (verified against `config.rs` dumps in both crates).

---

## 0. THE CREDENTIAL SET (plug-and-go; each row closes a STUB)

| # | Credential | env names (exact) | Where to get it | One-shot verify after setting |
|---|---|---|---|---|
| 1 | Trader wallet + testnet MON | (manual chain ops only; key never needed at runtime) | https://faucet.monad.xyz | `cast block-number --rpc-url https://testnet-rpc.monad.xyz` |
| 2 | Perpl testnet API key | `PERPL_API_KEY`, `PERPL_API_KEY_SECRET`, `PERPL_ENV=testnet` | https://testnet.perpl.xyz/apikeys (needs §2 account) | `cargo run --bin read-positions` -> live positions (STUB-09) |
| 3 | Qwen (Alibaba Model Studio) | `QWEN_API_KEY` (+`QWEN_BASE_URL`, `QWEN_MODEL` overrides) | Model Studio console | `./scripts/live-qwen-probe.sh` then `cargo run --bin brain_eval -- --out docs/evidence/p07-brain-eval.txt` (STUB-01) |
| 4 | Kimi (Moonshot) | `KIMI_API_KEY` (+`KIMI_BASE_URL`, `KIMI_MODEL`) | https://platform.moonshot.ai | `FORCE_PROVIDER_FAIL=qwen cargo run --bin brain_eval` -> `provider=kimi` (STUB-02) |
| 5 | x402 payer wallet (USDC on Monad, ~$10, SEPARATE) | `NANSEN_PAYER_KEY`, `NANSEN_PAYMENT_NETWORK=eip155:143` | `cast wallet new` + bridge/buy USDC at `0x754704Bc059F8C67012fEd69BC8A327a5aafb603` | `cargo run --bin test-nansen -- --address <wallet>` -> first paid call + tx (STUB-16) |
| 6 | Anchor signer wallet (testnet MON for gas) | `RPC_SIGNER_KEY`, `ANCHOR_CONTRACT_ADDRESS` (after deploy), `ENABLE_ANCHOR=true` | `cast wallet new` + faucet | `forge create ... contracts/src/SentinelAuditAnchor.sol` then `cargo run --bin audit-verify` live (STUB-17) |
| 7 | Telegram bot | `TELOXIDE_TOKEN`, `TELEGRAM_ALLOWED_USER_IDS`, `TELEGRAM_APPROVAL_CHAT_ID` | @BotFather + @userinfobot | walk `docs/telegram-manual-test.md` (14 rows) (STUB-18) |
| 8 | Dashboard admin key (any random string) | `DASHBOARD_ADMIN_KEY` | `openssl rand -hex 16` | `curl -X POST 'localhost:8080/api/pause?key=...'` -> `{"paused":true}` |
| 9 | Envio account | `ENVIO_API_TOKEN` (CLI), `ENVIO_GRAPHQL_ENDPOINT` (app) | https://envio.dev/app/api-tokens | `envio-cloud login` -> deploy -> endpoint answers a query (STUB-20) |
| 10 | Chainlink CRE account | CLI login (optional `CRE_API_KEY`) | `cre login` | `cre workflow simulate sentinel-deadman --non-interactive --trigger-index 0 --target local-simulation` (STUB-03) |
| 11 | Railway account | CLI login; volume mounted at `/app/data` | `railway login` | `railway up` -> `curl https://<app>/healthz` (STUB-24) |

Note: the **breaker** reads the same Telegram token via `TELOXIDE_TOKEN` (alias
`TELEGRAM_BOT_TOKEN` also accepted) and `TELEGRAM_APPROVAL_CHAT_ID`; one value configures both processes.

---

## 1. Monad Testnet network + gas tokens

1. Add Monad Testnet to your wallet (MetaMask/Rabby/etc.):
   - Network name: `Monad Testnet` · RPC: `https://testnet-rpc.monad.xyz` · Chain ID: `10143`
   - Currency: `MON` · Explorer: `https://testnet.monadexplorer.com`
   (One-click page: https://faucet.monad.xyz/add-network)

2. Get testnet MON: **https://faucet.monad.xyz** (connect X/Discord for a bigger drip).
   Alternative: https://www.alchemy.com/faucets/monad-testnet (1 MON / 24 h, no account).

## 2. Perpl testnet exchange account (REQUIRED before API orders work)

**Why:** API auth alone is not enough — the Exchange needs an on-chain account
(`createAccount`) AND order forwarding enabled (`allowOrderForwarding`). Without the
account, authenticated calls return 404; without forwarding, orders fail with `sr: 34`.

Easiest path (UI):
1. Open **https://testnet.perpl.xyz** and connect the wallet.
2. Use **"Deposit to Enable trading"** — creates the exchange account with the first
   deposit. Testnet minimum account open amount: **100 AUSD** (`min_account_open_amount =
   100000000`, 6 decimals; live context).
3. Enable **One-Click Trading** (user settings) — calls `allowOrderForwarding(true)`.
4. Get testnet collateral (AUSD, token `0xa9012a055bd4e0edff8ce09f960291c09d5322dc`).

Manual / fallback path (Foundry at `~/.local/bin`):
```bash
export RPC_URL=https://testnet-rpc.monad.xyz
export EXCHANGE=0x1964c32f0be608e7d29302aff5e61268e72080cc
export USD_TOKEN=0xa9012a055bd4e0edff8ce09f960291c09d5322dc   # testnet AUSD (6 dec)
export WALLET=0xYOUR_WALLET
cast call --from $WALLET $EXCHANGE "getAccountByAddr(address)(uint256)" $WALLET --rpc-url $RPC_URL
# if none: approve + createAccount (100.000000 AUSD) then allowOrderForwarding(true)
```

## 3. Perpl testnet API key

1. **https://testnet.perpl.xyz/apikeys** (account first).
2. Create a key with scope **read + trade**. The UI shows the token and the **Ed25519
   private key (hex, 32 bytes)** — shown ONCE; store both.
3. `.env`: `PERPL_API_KEY=`, `PERPL_API_KEY_SECRET=` (hex, 0x optional), `PERPL_ENV=testnet`.

## 4. Seed position (for the whole build + demo)

1. On https://testnet.perpl.xyz open **one small isolated ETH position** (market `32`):
   ~0.05 ETH at 2-5x leverage.
2. Screenshot the position page (entry, liq price, collateral) as evidence.

## 5. Telegram bot

1. **@BotFather** -> `/newbot` -> token. 2. Numeric id from **@userinfobot**.
3. `.env`: `TELOXIDE_TOKEN=`, `TELEGRAM_ALLOWED_USER_IDS=`, `TELEGRAM_APPROVAL_CHAT_ID=` (optional).

## 6. Alibaba Qwen (DashScope / Model Studio)

1. Alibaba Cloud Model Studio account + API key.
2. `.env`: `QWEN_API_KEY=`; try `QWEN_BASE_URL=https://dashscope-intl.aliyuncs.com/compatible-mode/v1`
   (else `https://dashscope.aliyuncs.com/compatible-mode/v1`); `QWEN_MODEL=qwen3.8-max`.
   The probe script picks the working base; model id env-overridable if the alias differs.

## 7. Kimi (Moonshot)

1. Key at https://platform.moonshot.ai. 2. `.env`: `KIMI_API_KEY=`,
   `KIMI_BASE_URL=https://api.moonshot.ai/v1`, `KIMI_MODEL=` (confirm exact kimi-family string on first call).

## 8. x402 payer wallet (Nansen micropayments) — SEPARATE low-balance wallet

1. `cast wallet new` (key ONLY into `.env`). 2. Fund with **USDC on Monad mainnet**
   (~$10): `0x754704Bc059F8C67012fEd69BC8A327a5aafb603` (eip155:143).
3. `.env`: `NANSEN_PAYER_KEY=0x<private key>`, `NANSEN_PAYMENT_NETWORK=eip155:143`.
   Promo (`X-Payer-Address`, 50% off first 100 settled calls) was not advertised in
   `/.well-known/x402`; we still send the header (STUB-04).

## 9. Anchor signer wallet (on-chain audit trail)

1. `cast wallet new` -> `RPC_SIGNER_KEY` (testnet MON from the faucet for gas).
   (May reuse the trader wallet if you prefer one key fewer.)
2. Deploy once the wallet is funded:
   `forge create --rpc-url https://testnet-rpc.monad.xyz --private-key $RPC_SIGNER_KEY contracts/src/SentinelAuditAnchor.sol:SentinelAuditAnchor`
3. `.env`: `ANCHOR_CONTRACT_ADDRESS=0x...`, `ENABLE_ANCHOR=true`, `HEARTBEAT_INTERVAL_SECS=120`.
   Verify: `cargo run --bin audit-verify` (live cross-check) -> `docs/evidence/p10-audit-verify.{txt,png}`.

## 10. Envio + CRE + Railway accounts

- **Envio**: token at https://envio.dev/app/api-tokens -> `indexer/.env` `ENVIO_API_TOKEN`;
  `envio-cloud login` then deploy; put the public GraphQL URL into `.env`
  `ENVIO_GRAPHQL_ENDPOINT` (the Rust client feature-degrades to off while unset).
- **CRE**: `cre login` -> `cd workflow-cre && cre workflow simulate sentinel-deadman --non-interactive --trigger-index 0 --target local-simulation`
  (stale emulator at `sentinel-deadman/tools/stale-breaker-emulator.ts`); then
  `cre workflow supported-chains` + registration per SPEC-P14 §6.
- **Railway**: `railway login` -> link repo -> add volume at `/app/data` -> vars from
  `.env.example` -> `railway up` -> verify `/healthz` + dashboard (RUNBOOK §7).

## 11. ZERO -> LIVE EVIDENCE (ordered; ~45-60 min once the keys are in)

1. `cargo run --bin read-positions` -> live testnet positions (closes STUB-09).
2. `./scripts/live-qwen-probe.sh` -> `cargo run --bin brain_eval -- --out docs/evidence/p07-brain-eval.txt`
   (real Qwen scoreboard; wants >=10/12 on the 12 core scenarios).
3. `FORCE_PROVIDER_FAIL=qwen cargo run --bin brain_eval` -> live Kimi failover capture (STUB-02).
4. `EXECUTION_MODE=TESTNET cargo run --bin test-execution` -> first live reduce + explorer link
   -> `docs/evidence/p05-live-reduce.png` (STUB-12).
5. Anchor: fund + `forge create` + run the daemon -> `cargo run --bin audit-verify` (STUB-17).
6. `cargo run --bin test-nansen -- --address <payer>` -> first real x402 payment (STUB-16).
7. Telegram: daemon + `docs/telegram-manual-test.md` walk + screenshots (STUB-18).
8. Railway deploy -> `/healthz` + dashboard URL (STUB-24); Envio cloud deploy (STUB-20);
   CRE login + simulate (STUB-03).
9. Refresh `docs/evidence/p01-verify-latest.txt` (`bash scripts/verify-facts.sh`) and re-run
   `bash scripts/check-links.sh` + the full suite before the Oct 12 freeze.

---

### Status log
| Step | Done | Date | Notes |
|---|---|---|---|
| 1 Monad network + MON | | | |
| 2 Exchange account + forwarding | | | |
| 3 API key | | | |
| 4 Seed position | | | |
| 5 Telegram | | | |
| 6 Qwen key | | | |
| 7 Kimi key | | | |
| 8 x402 wallet | | | |
| 9 Envio token | | | |
| 10 Anchor signer wallet + deploy | | | |
| 11 Dashboard admin key | | | |
| 12 CRE login | | | |
| 13 Railway login + deploy | | | |
| 14 Zero->live evidence pass | | | |
