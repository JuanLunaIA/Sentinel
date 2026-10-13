# Configuration

Every setting Sentinel reads, what it does, and when you would change it. All of
them live in one file: **`.env`** at the repository root. There is no second
place, no database, and no config service.

Start from the template:

```bash
cp .env.example .env
```

The template is fully commented, and the placeholder values pass validation on
purpose so a fresh clone runs. You only edit what you need.

## How to think about the settings

Four groups, in the order you will care about them:

1. **What it protects and how cautious it is** — the risk policy. Change these
   first; they are the ones that change behavior.
2. **How it connects** — exchange, AI providers, Telegram. Needed only to go
   live.
3. **Where it runs and what it writes** — mode, ports, paths, logs.
4. **Switches** — turn whole subsystems on and off.

A rule that applies to all of them: **secrets are read only through
`config::SecretString`**, which has a hand-written redacting `Debug`, so a key
never reaches a log line even if something fails while formatting. Never commit
`.env`; it is already in `.gitignore`.

---

## 1. Risk policy — the settings that decide behavior

These are the knobs most people want. They are plain numbers with no formatting
tricks.

| Setting | Default | What it means |
|---|---|---|
| `RISK_SOFT_PCT` | `25` | Distance-to-liquidation at or above this is **green**. Nothing to do. |
| `RISK_WARN_PCT` | `15` | At or above this (and below soft) is **yellow**: watch, and the AI gets consulted. |
| `RISK_HARD_PCT` | `8` | At or above this (and below warn) is **orange**: act. Below it is **red**. |
| `REFLEX_REDUCE_FRACTION` | `0.5` | How much of the position a yellow/orange reflex action reduces. |
| `REFLEX_ORANGE_FRACTION` | `0.25` | The smaller fraction used for orange. |
| `REFLEX_COOLDOWN_SECS` | `600` | Minimum seconds between reflexive actions on the same market. Stops it from nibbling continuously. |
| `MAX_ORDER_SIZE_USD` | `5000` | Ceiling on a single action's notional. |
| `MAX_DAILY_ACTIONS` | `20` | Ceiling on actions per day. |
| `MARKET_ALLOWLIST` | `32,16` | Which markets it may touch, by numeric id. **Testnet** ETH=32, BTC=16. **Mainnet** ETH=20, BTC=1. |
| `REQUIRE_APPROVAL_ABOVE_USD` | `2500` | Above this notional, a human must approve over Telegram. |
| `IDEMPOTENCY_WINDOW_SECS` | `60` | How long a repeated order is treated as the same order, so a retry cannot double-fill. |
| `STALE_DATA_ALERT_SECS` | `30` | How old the market feed may get before it alerts. |

The distance percentages are measured from the liquidation price, not from the
entry price, and they are recomputed every tick from the position and the
market. If you are unsure what to set, leave them: the defaults are the
conservative direction.

**Changing these changes the backtest numbers.** That is the point of having
them. If you change a threshold and re-run `backtest`, the reports in `docs/`
will differ from the committed ones, which is expected.

### A note on the four levels

The levels answer one question only: *how far can the price move against me
before I am liquidated?* They never try to predict whether a price will go up
or down. Green means there is room. Red means there is almost none. That is the
entire model, and [HOW-IT-THINKS.md](HOW-IT-THINKS.md) explains why that is
deliberate.

Since v1.1.0 the thresholds can also adjust themselves to volatility
(`--adaptive`), widening in calm markets and tightening in a dislocation. It is
opt-in and off by default, so the table above is what runs unless you ask
otherwise.

---

## 2. Connections — needed only to go live

### Perpl, the exchange

| Setting | Notes |
|---|---|
| `PERPL_ENV` | `testnet` or `mainnet`. This one selects defaults for everything below. |
| `PERPL_API_KEY` | The key from your Perpl account. Scope it to read + trade. |
| `PERPL_API_KEY_SECRET` | The 32-byte Ed25519 seed, shown once when the key is created. 64 hex characters. **It is shown once. If you lost it, make a new key.** |
| `PERPL_ACCOUNT` | The wallet address that owns the exchange account. |

Everything else Perpl-related has a correct default per network and only exists
as an override: `PERPL_API_URL`, `PERPL_WS_URL`, `PERPL_RPC_URL`,
`PERPL_EXCHANGE_ADDRESS`, `PERPL_COLLATERAL_TOKEN`. The committed values are
verified in [FACTS.md](FACTS.md) §1.

### The two AI providers

You need at least one. Qwen is primary, Kimi is the fallback, and the failover
is tested by force-failing the primary.

| Setting | Default | Notes |
|---|---|---|
| `QWEN_API_KEY` | — | Alibaba Model Studio / DashScope. |
| `QWEN_BASE_URL` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | If your account is on the mainland endpoint, use `dashscope.aliyuncs.com`. The workspace's own `.env` points at `maas.qwencloudapi.com`. **If calls fail with an HTTP error, this URL is the first thing to check.** |
| `QWEN_MODEL` | `qwen3.8-max` | |
| `QWEN_MAX_TOKENS` | `4000` | |
| `QWEN_TEMPERATURE` | `0.1` | Low on purpose: this is a decision, not a story. |
| `KIMI_API_KEY` | — | Moonshot. |
| `KIMI_BASE_URL` | `https://api.moonshot.ai/v1` | |
| `KIMI_MODEL` | `kimi-k3` | Confirm the exact model string against your account at the first keyed call. |

Both providers are asked for **strict JSON**, and the response is parsed and
validated. A provider that returns prose instead gets one repair nudge, and then
the failover takes over. The commitment recorded in the audit journal covers the
model id, the endpoint, the sampling parameters and the raw completion, so you
can always tell which one actually answered.

### Telegram (human approval)

| Setting | Notes |
|---|---|
| `TELOXIDE_TOKEN` | Bot token from @BotFather. |
| `TELEGRAM_ALLOWED_USER_IDS` | Comma-separated numeric user ids allowed to approve. **Anyone not on this list is ignored**, which is what stops a stranger who finds your bot from approving a trade. |
| `TELEGRAM_APPROVAL_CHAT_ID` | Optional. Where approval requests are sent. |

### Nansen (paid market intelligence, x402)

Optional. If enabled without a funded payer key, it fails loudly rather than
pretending.

| Setting | Default | Notes |
|---|---|---|
| `NANSEN_BASE_URL` | `https://api.nansen.ai` | |
| `NANSEN_PAYER_KEY` | — | Private key of a **separate, low-balance** wallet. Do not reuse your trading wallet. |
| `NANSEN_PAYMENT_NETWORK` | `eip155:143` | Monad mainnet USDC. |
| `NANSEN_CACHE_TTL_SECS` | `300` | Cache window, so the same question is not paid for twice. |
| `NANSEN_MAX_CALLS_PER_HOUR` | `40` | Spend ceiling. |

### On-chain audit anchor

| Setting | Notes |
|---|---|
| `ANCHOR_CONTRACT_ADDRESS` | The `SentinelAuditAnchor` deployment. |
| `RPC_SIGNER_KEY` | Private key of the wallet that sends anchors. Keep it funded with a small amount; it only pays gas. |
| `ANCHOR_FROM_SEQ` | Where to resume anchoring if the contract cannot be read. |

Anchoring is what makes the journal tamper-evident to a third party. Without it
the journal is still hash-chained and verifiable locally, so the product works;
you just lose the public timestamp.

---

## 3. Where it runs and what it writes

| Setting | Default | Notes |
|---|---|---|
| `EXECUTION_MODE` | `DRY_RUN` | `DRY_RUN`, `TESTNET`, or `MAINNET`. Live modes must match `PERPL_ENV`. |
| `I_UNDERSTAND_MAINNET_RISK` | `no` | Mainnet requires the literal acknowledgement. This is deliberate friction on the one setting that can lose real money. |
| `PORT` | `8080` | Health endpoint and dashboard. |
| `LOG_DIR` | `./logs` | |
| `LOG_FORMAT` | `pretty` | Use `json` for machine consumption. |
| `RUST_LOG` | `sentinel=debug,tower_http=info` | Standard Rust tracing filter syntax. |
| `HEARTBEAT_INTERVAL_SECS` | `120` | How often the status file is refreshed. |
| `HEARTBEAT_PATH` | `data/heartbeat.json` | Consumed by the dashboard resilience panel and by the dead-man's switch. |
| `AUDIT_DIR` | `data/audit` | Where the hash-chained journal and the inference archive are written. **Back this up.** |
| `BACKTEST_REPORT_PATH` | `docs/backtest-report.json` | Which report the dashboard displays. |
| `DASHBOARD_ADMIN_KEY` | unset | Mutations (pause, kill switch) require it. If unset, mutations are disabled, which is the safe default for a public deployment. |
| `ENVIO_GRAPHQL_ENDPOINT` | unset | Your Envio Cloud deployment, for public journal queries. |

---

## 4. Switches

Turn whole subsystems off without removing their configuration:

| Setting | Default | Turns off |
|---|---|---|
| `ENABLE_REFLEX` | `true` | The deterministic protection. Think carefully before disabling this. |
| `ENABLE_STRATEGY` | `true` | The AI consultations. |
| `ENABLE_NANSEN` | `true` | Paid intelligence. |
| `ENABLE_ANCHOR` | `true` | On-chain anchoring. |

The dead-man's switch has its own block, all optional, and it **fail-fasts** if
you enable it without its required values: `BREAKER_RPC_URL`,
`BREAKER_ANCHOR_ADDRESS`, `BREAKER_GUARDIANS`, `BREAKER_ARM_SECRET`,
`BREAKER_HEARTBEAT_INTERVAL_SECS` (default `60`), `BREAKER_STALE_MULT`
(default `3`), `BREAKER_MODE` (default `dry_run`).

---

## 5. Advanced: the backtester's flags

Two opt-in flags, and their current state matters:

| Flag | State |
|---|---|
| `--adaptive` | **Shipped.** Volatility-adaptive tier bands. |
| `--cascade` | **Experimental and measured harmful. Do not enable.** On the contention family it turns +$50.18 into −$263.00. The reason is in [evidence/p28-contention-family.txt](evidence/p28-contention-family.txt). |

```bash
cargo run --release --bin backtest -- --adaptive
```

Both default to off, so the numbers in `docs/backtest-report.md` are the
default-path numbers.

---

## Changing a setting safely

1. Edit `.env`.
2. Run the zero-key replay to confirm it still starts:
   `cargo run --bin sentinel -- --replay tests/fixtures/perpl/crash-scenario.jsonl`
3. If you changed a risk threshold, re-run the backtest and compare:
   `cargo run --release --bin backtest`
4. Only then consider a live mode.

Nothing in step 1 touches the network. The daemon validates its whole
configuration at startup and refuses to start on an inconsistent combination
(for example a live mode whose `PERPL_ENV` does not match), which is why it is
safe to experiment.
