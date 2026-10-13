# Realistic bootstrap scenarios (`tests/scenarios-realistic/`)

Generated replays for the P13 backtester, built to answer one question: how
does the engine behave on price paths that look like the real Perpl testnet
tape, instead of hand-drawn ones?

Run them with:

```bash
python3 tools/scenarios/generate_realistic.py       # regenerate (seeded, deterministic)
cargo run --release --bin backtest -- --scenarios-dir tests/scenarios-realistic --out docs/backtest-report-realistic
```

## Method

**1. Real tape.** The return series is real: 960 one-minute candles for ETH
(market 32) and BTC (market 16), fetched live from `https://testnet.perpl.xyz/api`
(`/v1/market-data/:id/candles/60/:from-:to`) and stored in
`docs/evidence/p21-perpl-real-candles.json`. The window captured a live -3.1%
ETH drawdown.

Measured properties of that tape (the numbers the generator has to respect):

| series | σ/min | annualized vol | skew | excess kurtosis | jumps > 4σ | largest |
|---|---|---|---|---|---|---|
| ETH 1m | 0.000775 | 56.2% | -0.80 | +10.22 | 6 | 7.6σ |
| BTC 1m | 0.000522 | 37.8% | -0.08 | +3.20 | 5 | 5.5σ |
| SOL 1m | 0.001067 | 77.3% | -0.51 | +8.61 | 10 | 6.7σ |

Cross-asset correlation (ETH/BTC, 1m): **+0.759**. Volatility clustering
(acf1 of |returns|): **+0.262**.

**2. Block bootstrap.** Paths are built by sampling contiguous 30-bar blocks
of the real returns with replacement. Block sampling keeps the empirical
properties above without assuming any distribution: the generated paths carry
the same fat tails, the same negative skew and the same clustered volatility
as the tape, because they are literal slices of it.

**3. Cross-asset structure.** Multi-asset scenarios sample the same block
indices for both assets, so the real +0.76 ETH/BTC correlation drives both
legs together. No correlation model is involved.

**4. Regime injections.** Each scenario adds one calibrated event on top of
its bootstrapped path. Magnitudes come from the observed tails, not from
round numbers:

| regime | what it does | calibration |
|---|---|---|
| `drift_down` | sustained bleed over the whole path | 3.0% to 5.5%, matching the live session |
| `quiet` | returns scaled x0.55 | low-vol regime |
| `gap_down` | one tail minute | 5.5σ to 7.5σ (observed max was 7.6σ) |
| `crash` | fast multi-bar decline | 7% to 14% over 8-20 bars |
| `whipsaw` | crash then full recovery | 6% to 12% down, then back |
| `squeeze_up` | upward move against shorts | 5% to 10% |
| `vol_spike` | returns x3-5 for 20-40 bars | matches acf +0.26 clustering |
| `gap_through_liq` | instant -16% gap | honest negative control |

**5. Positions and policy.** Prices start from the last real close of each
tape (ETH 2474.51, BTC 82654.0) and leverage runs 3x to 12x, so the implied
liquidation distances (-3.5% at 12x to -47% at 2x) sit inside the range of
moves the real tape actually produces. Policy caps scale with position
notional the way a real deployment would; the engine's defaults ($1000 order
cap, $500 approval threshold) are sized for the tiny hand-authored scenarios,
not for a book of this size.

## What each scenario probes

| scenario | tape | position | expected shape |
|---|---|---|---|
| `rb-01-drift-down-10x` | real drift | ETH 10x long | trim before the liquidation band |
| `rb-02-quiet-range-12x` | vol damped | ETH 12x long | act only if the band is truly approached |
| `rb-03-tail-gap-8x` | 5.5-7.5σ minute | ETH 8x long | react after the gap, survive |
| `rb-04-whipsaw-recovery-6x` | crash + recovery | ETH 6x long | protective trim, cost measured |
| `rb-05-slow-bleed-4x` | long decline | ETH 4x long | early trim, no liquidation |
| `rb-06-correlated-eth-btc-5x` | correlated crash | ETH+BTC 5x | both legs defended |
| `rb-07-short-squeeze-10x` | up-squeeze | ETH 10x short | mirror case |
| `rb-08-vol-cluster-8x` | vol spike | ETH 8x long | survive a violent window |
| `rb-09-noop-flat-3x` | flat | ETH 3x long | do nothing (false-positive test) |
| `rb-10-gap-through-liq-12x` | instant -16% | ETH 12x long | negative control: unsavable |
| `rb-11-trim-then-recover-5x` | drawdown + recovery | ETH 5x long | cost of being protected |
| `rb-12-two-leg-portfolio` | correlated drift | ETH 8x + BTC 6x | multi-market allowlist, daily cap |

## What the first run found

The suite has been run and the findings are written up in
[`docs/evidence/p21-realistic-bootstrap.txt`](../../docs/evidence/p21-realistic-bootstrap.txt).
Headlines:

- 12 scenarios, $38,533 notional, **$394.02 saved (1.02%)**, 1 baseline
  liquidation avoided, 0 policy violations.
- **Design issue:** with absolute tier bands (25/15/8 percent), any position
  opened at ~5x or higher starts inside a band, so 10 of 12 scenarios trimmed
  at the first tick without the tape moving. Reported, not patched: fixing it
  changes the frozen P13 numbers and is a policy decision.
- **Honest negative:** the whipsaw scenario saved **-$3.21**: the trim cost
  money when the market recovered. Protection is not free.
- **Negative control:** the gap scenario loses ~$100 with or without the
  guardian ($97.83 vs $103.10). A gap through liquidation is not recoverable;
  the small delta is the head start from the tick-0 trim.
- **Correct no-ops verified:** the 4x and 3x scenarios (21% and 30% from
  liquidation) produced zero actions on real-volatility tape.

## Honesty notes

- These files are labelled `realistic-bootstrap`, distinct from `synthetic`
  (hand-authored), `recorded` (real marks replayed verbatim) and
  `reconstructed`. The label says exactly what produced the path.
- The bootstrap resamples a ~16-hour window. It inherits that window's
  regime: a quiet 16 hours yields mostly quiet paths. That is a property of
  block resampling, and the regime injections exist to compensate.
- Prices are re-anchored to the last real close, so the absolute levels match
  the live venue, but no generated path is a market forecast. The scenarios
  measure engine behaviour, not future prices.
- Seeded and deterministic: `--seed` reproduces the suite byte for byte.
