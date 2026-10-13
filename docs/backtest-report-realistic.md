# Sentinel backtest report

Across 12 scenarios representing $38533.499 notional, Sentinel preserved $394.023198812652825 (1.02%); baseline liquidations: 1 -> with Sentinel: 0

## Aggregate

| metric | value |
|---|---|
| scenarios | 12 |
| total notional (USD) | 38533.499 |
| total saved (USD) | 394.023198812652825 |
| saved (% of notional) | 1.02 |
| baseline liquidations | 1 |
| sentinel liquidations | 0 |

## Scenarios

### rb-01-drift-down-10x [realistic-bootstrap]

- ticks: 240
- actions: 2
- baseline loss: $35.844 USD; sentinel loss: $14.569441870805 USD; saved: $21.274558129195 USD; sim fees: $0.213110870805 USD; scenario notional: $989.804 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.2 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.200 @ 2471.76576 (sim-32-1, fee $0.17055183744) |
| 1791854580000 | 32 | Orange | REFLEX | allow | order close-long 0.05 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.050 @ 2467.19034 (sim-32-2, fee $0.042559033365) |

### rb-02-quiet-range-12x [realistic-bootstrap]

- ticks: 240
- actions: 2
- baseline loss: $12.8 USD; sentinel loss: $6.54257489428 USD; saved: $6.25742510572 USD; sim fees: $0.42639889428 USD; scenario notional: $1979.608 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.4 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.400 @ 2472.17535 (sim-32-1, fee $0.3411601983) |
| 1791855000000 | 32 | Orange | REFLEX | allow | order close-long 0.1 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.100 @ 2470.68684 (sim-32-2, fee $0.08523869598) |

### rb-03-tail-gap-8x [realistic-bootstrap]

- ticks: 240
- actions: 1
- baseline loss: $76.974 USD; sentinel loss: $39.682942431525 USD; saved: $37.291057568475 USD; sim fees: $0.255787431525 USD; scenario notional: $1484.706 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 1; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.3 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.300 @ 2471.37615 (sim-32-1, fee $0.255787431525) |

### rb-04-whipsaw-recovery-6x [realistic-bootstrap]

- ticks: 240
- actions: 2
- baseline loss: $27.33 USD; sentinel loss: $30.5411303417617 USD; saved: $-3.2111303417617 USD; sim fees: $0.3679682017617 USD; scenario notional: $2474.51 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Orange | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.250 @ 2472.35517 (sim-32-1, fee $0.2132406334125) |
| 1791859560000 | 32 | Orange | REFLEX | allow | order close-long 0.187 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.187 @ 2398.31928 (sim-32-2, fee $0.1547275683492) |

### rb-05-slow-bleed-4x [realistic-bootstrap]

- ticks: 480
- actions: 0
- baseline loss: $291.125 USD; sentinel loss: $291.125 USD; saved: $0 USD; sim fees: $0 USD; scenario notional: $6186.275 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 0; policy violations: 0
- note: label: realistic-bootstrap

### rb-06-correlated-eth-btc-5x [realistic-bootstrap]

- ticks: 480
- actions: 4
- baseline loss: $525.939 USD; sentinel loss: $392.49070155572733 USD; saved: $133.44829844427267 USD; sim fees: $0.71415806972733 USD; scenario notional: $4954.13 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 4; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Orange | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.250 @ 2470.04748 (sim-32-1, fee $0.21304159515) |
| 1791863580000 | 32 | Orange | REFLEX | allow | order close-long 0.187 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.187 @ 2272.98474 (sim-32-2, fee $0.1466416105011) |
| 1791863670000 | 16 | Orange | REFLEX | allow | order close-long 0.0075 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.00750 @ 80709.6096 (sim-16-1, fee $0.20883611484) |
| 1791864030000 | 16 | Orange | REFLEX | allow | order close-long 0.00562 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.00562 @ 75114.1107 (sim-16-2, fee $0.14563874923623) |

### rb-07-short-squeeze-10x [realistic-bootstrap]

- ticks: 240
- actions: 2
- baseline loss: $0 USD; sentinel loss: $0.2132158182525 USD; saved: $-0.2132158182525 USD; sim fees: $0.2132158182525 USD; scenario notional: $989.804 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.2 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.200 @ 2472.10542 (sim-32-1, fee $0.17057527398) |
| 1791854880000 | 32 | Orange | REFLEX | allow | order close-long 0.05 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.050 @ 2471.91561 (sim-32-2, fee $0.0426405442725) |

### rb-08-vol-cluster-8x [realistic-bootstrap]

- ticks: 240
- actions: 1
- baseline loss: $48.132 USD; sentinel loss: $25.4263132330375 USD; saved: $22.7056867669625 USD; sim fees: $0.2984307330375 USD; scenario notional: $1732.157 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 1; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.35 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.350 @ 2471.47605 (sim-32-1, fee $0.2984307330375) |

### rb-09-noop-active-3x [realistic-bootstrap]

- ticks: 240
- actions: 0
- baseline loss: $109.2 USD; sentinel loss: $109.2 USD; saved: $0 USD; sim fees: $0 USD; scenario notional: $7423.53 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 0; policy violations: 0
- note: label: realistic-bootstrap

### rb-10-gap-through-liq-12x [realistic-bootstrap]

- ticks: 240
- actions: 2
- baseline loss: $103.1 USD; sentinel loss: $97.8267009304375 USD; saved: $5.2732990695625 USD; sim fees: $0.3932384304375 USD; scenario notional: $1237.255 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- note: label: realistic-bootstrap
- note: sentinel position crossed its liq price at 1791854580000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854640000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854700000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854760000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854820000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854880000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791854940000 ms (formula-only accounting, P13 §5)
- note: sentinel position crossed its liq price at 1791855000000 ms (formula-only accounting, P13 §5)

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.250 @ 2469.78774 (sim-32-1, fee $0.213019192575) |
| 1791855000000 | 32 | Red | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | red tier: still in breach after reduce: filled 0.250 @ 2089.49841 (sim-32-2, fee $0.1802192378625) |

### rb-11-trim-then-recover-5x [realistic-bootstrap]

- ticks: 480
- actions: 2
- baseline loss: $166.86 USD; sentinel loss: $141.594442806885 USD; saved: $25.265557193115 USD; sim fees: $0.431875806885 USD; scenario notional: $2969.412 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791856860000 | 32 | Orange | REFLEX | allow | order close-long 0.3 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.300 @ 2471.85567 (sim-32-1, fee $0.255837061845) |
| 1791864840000 | 32 | Orange | REFLEX | allow | order close-long 0.225 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.225 @ 2267.80992 (sim-32-2, fee $0.17603874504) |

### rb-12-two-leg-portfolio [realistic-bootstrap]

- ticks: 600
- actions: 5
- baseline loss: $387.539 USD; sentinel loss: $241.607337304635645 USD; saved: $145.931662695364355 USD; sim fees: $1.033639045635645 USD; scenario notional: $6112.308 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 4; policy violations: 0
- note: label: realistic-bootstrap

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791854400000 | 32 | Red | REFLEX | allow | order close-long 0.4 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.400 @ 2472.03549 (sim-32-1, fee $0.34114089762) |
| 1791854400000 | 16 | Orange | REFLEX | allow | orange tier: de-risking reduce: executor error: perpl: order: no mark price for market 16 |
| 1791855000000 | 16 | Orange | REFLEX | allow | order close-long 0.0125 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.01250 @ 82428.2892 (sim-16-1, fee $0.355471997175) |
| 1791864870000 | 16 | Orange | REFLEX | allow | order close-long 0.00937 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.00937 @ 79431.1893 (sim-16-2, fee $0.256773234090645) |
| 1791868320000 | 32 | Orange | REFLEX | allow | order close-long 0.1 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.100 @ 2326.17150 (sim-32-2, fee $0.08025291675) |

