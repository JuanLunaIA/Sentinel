# Sentinel — Architecture (v0, P02)

Status: skeleton lands in P02; extended by later prompts. Verified integration
facts live in `docs/FACTS.md`; human setup steps in `docs/SETUP-MANUAL.md`.

## What Sentinel is

Sentinel is a **verifiable AI risk guardian** for isolated-margin perpetual
futures on Perpl (Monad). It is not a trading bot: it never opens or grows
exposure. It watches, decides, and defensively de-risks — with every decision
provable after the fact.

## Architecture invariants (never violated)

1. The reflex core never calls an LLM. The LLM never bypasses the policy gate.
2. Audit-before-action: journal the intent (hash-chained) BEFORE execution;
   append the outcome after.
3. All external I/O lives behind async traits → mockable/recordable.
   `DRY_RUN` is a first-class execution mode that simulates fills locally.
4. The pure core crate (`sentinel-core`: risk math, policy, audit chaining,
   types) has ZERO network dependencies.
5. Never log private keys, API-key secrets, or signed payloads. Redact in
   `Debug` impls (see `config::SecretString`).

## System diagram (v0)

```
                        ┌────────────────────────────────────────────────────────┐
                        │                    SENTINEL (Rust)                     │
                        │                                                        │
   Perpl REST+WS  ─────▶│  perpl/     perception: Ed25519 auth, snapshots,       │
   (mt:9 mrk, mt:19/21, │             market-state + account streams             │
    mt:26 positions)    │        │                                               │
                        │        ▼                                               │
                        │  sentinel-core (pure)                                  │
                        │   • risk.rs  reflex math: distance-to-liq, tiers       │
                        │   • policy.rs  caps, allowlists, approvals, kill       │
                        │   • audit.rs  hash chain (tamper-evident)              │
                        │        │                                               │
                        │        ▼                                               │
                        │  execution: DryRun │ Testnet │ Mainnet                 │
                        │   (reduce-only CloseLong/CloseShort, idempotent rq)    │
                        │        │                                               │
                        │        ▼                                               │
                        │  audit journal ─▶ SentinelAuditAnchor (Monad)          │
                        │        ▲                          ▲                    │
                        │        │                          │  Heartbeat         │
   Qwen 3.8-Max ───────▶│  strategy brain (P07)      Chainlink CRE (P14):        │
   Kimi K3 fallback ───▶│  + Nansen x402 data (P09)  dead-man's switch ─▶        │
                        │                            independent breaker         │
                        │  Telegram bot (P11)  ·  API + dashboard (P15)          │
                        └────────────────────────────────────────────────────────┘
                             ▲                                     ▲
                     Envio HyperIndex (P12)               Backtester/replay (P13)
                     Perpl events + anchors               "capital saved" report
```

## Execution modes

| Mode | Orders | Use |
|---|---|---|
| `DRY_RUN` | simulated fills at mark ± slippage bps | development, tests, demo replay |
| `TESTNET` | live reduce-only orders on Monad testnet | validation, judging demo |
| `MAINNET` | live reduce-only orders on Monad mainnet | gated by `I_UNDERSTAND_MAINNET_RISK=yes` |

Live modes must be consistent with `PERPL_ENV` (validated at load): `TESTNET` requires
`PERPL_ENV=testnet`, `MAINNET` requires `PERPL_ENV=mainnet`; `DRY_RUN` works on either.

## Crate layout

- `crates/sentinel-core` — pure domain core. Typed domain model (real units,
  not venue encodings), risk math, policy gate, audit chain. No I/O, no
  clock, `#![forbid(unsafe_code)]`.
- `crates/sentinel` — application: config (fail-fast, redacting), error
  hierarchy, telemetry, and — from later prompts — perception, brain,
  execution, bot, API, and the `sentinel` daemon binary.

## Conventions

- **Always latest versions.** Toolchains, crates and CLIs track the latest
  releases — even when an older version is quoted in any spec or prompt.
  Rust deps are resolved with `cargo add` (live crates.io); the toolchain
  file tracks `stable`. The MSRV floor (`rust-version`) only bounds
  compatibility (perpl-sdk pins rustc 1.97; alloy 2.x needs ≥ 1.94.1).
- Fail-fast config: `Config::load()` refuses to start on missing required
  variables, invalid values or broken invariants (`hard < warn < soft`,
  MAINNET acknowledgement, secret hex length).
- Evidence-first: every prompt's validation output is captured under
  `docs/evidence/` and mapped into `docs/FACTS.md` / `docs/STUBS.md`.
