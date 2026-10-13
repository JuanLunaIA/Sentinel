# Getting started

Five minutes, no accounts, no API keys, no Docker. All you need is the Rust
toolchain.

## 1. Check what you need

```bash
rustc --version && cargo --version
```

The exact toolchain is pinned in [../rust-toolchain.toml](../rust-toolchain.toml),
so `cargo` will fetch it for you the first time you build. If you do not have
Rust at all, install it from <https://rustup.rs> — one command, no options to
think about.

## 2. Get the code and a config file

```bash
git clone https://github.com/JuanLunaIA/Sentinel
cd Sentinel
cp .env.example .env
```

You do **not** need to edit `.env` for what follows. The shipped placeholder
values pass validation on purpose, so a smoke run works on a fresh clone. The
file exists so that when you do want to go live, every setting is already
documented in place.

## 3. Run the test suite (optional, about two minutes)

```bash
cargo test --workspace
```

This runs 44 suites and 955 tests. It is the same command CI runs, so a green
result here means a green result there.

## 4. Watch it make decisions

```bash
cargo run --bin sentinel -- --replay tests/fixtures/perpl/crash-scenario.jsonl
```

This feeds a recorded market crash through the real decision path, offline and
deterministic. Nothing is sent anywhere: no exchange, no AI provider, no chain.

While it runs, open <http://localhost:8080> in a browser. That is the actual
operator dashboard, serving live from the replay. You can also check the health
endpoint:

```bash
curl -s http://localhost:8080/healthz
```

The default pace is fast, so the dashboard updates in bursts. To watch it at a
human speed:

```bash
SENTINEL_MOCK_PACE=1 SENTINEL_MOCK_CAP_MS=3200 cargo run --bin sentinel -- --replay tests/fixtures/perpl/crash-scenario.jsonl
```

## 5. See the numbers

```bash
cargo run --release --bin backtest
```

This replays the full scenario suite and writes two reports:
[docs/backtest-report.md](backtest-report.md) for humans and
[docs/backtest-report.json](backtest-report.json) for machines. Open the
Markdown one: it has the per-scenario table and the aggregate line.

## What you just ran, and what you did not

You ran the **entire decision path** for real: risk measurement, tier
classification, the policy gate, the audit journal, and the dashboard. What you
did not run is the parts that need accounts, because those cost money or need
your identity:

| Not exercised | Why | How to enable |
|---|---|---|
| Live exchange orders | needs a Perpl account and API key | [SETUP-MANUAL.md](SETUP-MANUAL.md) §2–3 |
| The AI strategy brain | needs a model provider key | [SETUP-MANUAL.md](SETUP-MANUAL.md) §6–7 |
| Paid market intelligence | needs a funded x402 wallet | [SETUP-MANUAL.md](SETUP-MANUAL.md) §8 |
| On-chain audit anchoring | needs a funded wallet | [SETUP-MANUAL.md](SETUP-MANUAL.md) §9 |

The important part: **the protection does not depend on any of those.**
Protection runs in the reflex path, which is deterministic and has no network
call in it at all. The AI can be offline, wrong, or an account you never
created, and your position still gets protected. That is the design, not a
fallback.

## Next

- Every setting explained: [CONFIGURATION.md](CONFIGURATION.md)
- Going live, step by step: [SETUP-MANUAL.md](SETUP-MANUAL.md)
- Running it day to day: [RUNBOOK.md](RUNBOOK.md)
