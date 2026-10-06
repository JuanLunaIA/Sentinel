# Sentinel backtest report

Across 14 scenarios representing $60163.365 notional, Sentinel preserved $5972.13546111693125 (9.93%); baseline liquidations: 11 -> with Sentinel: 0

## Aggregate

| metric | value |
|---|---|
| scenarios | 14 |
| total notional (USD) | 60163.365 |
| total saved (USD) | 5972.13546111693125 |
| saved (% of notional) | 9.93 |
| baseline liquidations | 11 |
| sentinel liquidations | 0 |

## Scenarios

### black-swan-60 [synthetic]

- ticks: 31
- actions: 2
- baseline loss: $136 USD; sentinel loss: $69.73729426 USD; saved: $66.26270574 USD; sim fees: $0.44529426 USD; scenario notional: $1360 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240000000 | 32 | Red | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.250 @ 2717.280 (sim-32-1, fee $0.23436540) |
| 1791240600000 | 32 | Red | REFLEX | allow | order close-long 0.25 (slippage cap 10 bps) | red tier: still in breach after reduce: filled 0.250 @ 2445.552 (sim-32-2, fee $0.21092886) |

### correlated-dump [synthetic]

- ticks: 52
- actions: 4
- baseline loss: $706.4 USD; sentinel loss: $247.6447914593 USD; saved: $458.7552085407 USD; sim fees: $0.5087314593 USD; scenario notional: $3532 USD
- liquidations: baseline 2; sentinel 0; avoided 2
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240060000 | 32 | Orange | REFLEX | allow | order close-long 0.15 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.150 @ 2709.288 (sim-32-1, fee $0.140205654) |
| 1791240150000 | 16 | Orange | REFLEX | allow | order close-long 0.005 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.00500 @ 93706.200 (sim-16-1, fee $0.1616431950) |
| 1791240540000 | 32 | Orange | REFLEX | allow | order close-long 0.112 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.112 @ 2477.520 (sim-32-2, fee $0.0957313728) |
| 1791240930000 | 16 | Orange | REFLEX | allow | order close-long 0.00375 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.00375 @ 85914.000 (sim-16-2, fee $0.1111512375) |

### dust-position [synthetic]

- ticks: 11
- actions: 2
- baseline loss: $0.02 USD; sentinel loss: $0.0436036754 USD; saved: $-0.0236036754 USD; sim fees: $0.0009236754 USD; scenario notional: $2.72 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240000000 | 32 | Red | REFLEX | allow | red tier: first-breach reduce: sizing skipped (size below lot/min) |
| 1791240720000 | 32 | Red | REFLEX | allow | order close-long 0.001 (slippage cap 10 bps) | red tier: still in breach after reduce: filled 0.001 @ 2677.320 (sim-32-1, fee $0.0009236754) |

### flash-crash-30 [synthetic]

- ticks: 61
- actions: 2
- baseline loss: $108.8 USD; sentinel loss: $78.6867302504 USD; saved: $30.1132697496 USD; sim fees: $0.2990502504 USD; scenario notional: $1088 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240000000 | 32 | Red | REFLEX | allow | order close-long 0.3 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.300 @ 2717.280 (sim-32-1, fee $0.28123848) |
| 1791240240000 | 32 | Orange | REFLEX | allow | order close-long 0.025 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.025 @ 2065.1328 (sim-32-2, fee $0.0178117704) |

### funding-squeeze [synthetic]

- ticks: 25
- actions: 2
- baseline loss: $380.8 USD; sentinel loss: $206.46386697542375 USD; saved: $174.33613302457625 USD; sim fees: $0.27582472542375 USD; scenario notional: $1904 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- note: ignored event 'funding' at 1791243600000 ms (unmodeled in v1.0)
- note: event note: funding rate spike: longs pay; funding is not modeled in v1.0 (ignored per SPEC-P13 §3)
- note: ignored event 'funding' at 1791250800000 ms (unmodeled in v1.0)
- note: event note: second funding spike inside the squeeze; ignored

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791241800000 | 32 | Orange | REFLEX | allow | order close-long 0.175 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.175 @ 2698.96833 (sim-32-1, fee $0.16295021292375) |
| 1791261600000 | 32 | Orange | REFLEX | allow | order close-long 0.131 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.131 @ 2497.500 (sim-32-2, fee $0.1128745125) |

### gap-through-liq [synthetic]

- ticks: 12
- actions: 1
- baseline loss: $272 USD; sentinel loss: $12.9742341125 USD; saved: $259.0257658875 USD; sim fees: $0.1217341125 USD; scenario notional: $1360 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240060000 | 32 | Orange | REFLEX | allow | order close-short 0.125 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.125 @ 2822.820 (sim-32-1, fee $0.1217341125) |

### reconstructed-testnet-eth [reconstructed]

- ticks: 47
- actions: 1
- baseline loss: $13560 USD; sentinel loss: $11107.521013362375 USD; saved: $2452.478986637625 USD; sim fees: $1.472788362375 USD; scenario notional: $27000 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- note: label: reconstructed

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791267000000 | 32 | Orange | REFLEX | allow | order close-long 2.5 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 2.500 @ 1707.58071 (sim-32-1, fee $1.472788362375) |

### recorded-session-20261005 [recorded]

- ticks: 3
- actions: 0
- baseline loss: $0 USD; sentinel loss: $0 USD; saved: $0 USD; sim fees: $0 USD; scenario notional: $1358.645 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 0; policy violations: 0
- note: label: recorded

### recovery-v [synthetic]

- ticks: 13
- actions: 2
- baseline loss: $0 USD; sentinel loss: $93.3279756737 USD; saved: $-93.3279756737 USD; sim fees: $0.1904356737 USD; scenario notional: $2430 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 2; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791254400000 | 32 | Yellow | STRATEGY | allow | order close-long 0.027 (slippage cap 10 bps) | Yellow entry: trim 3% ahead of the 24h dip per the recorded decision trace: filled 0.027 @ 2537.460 (sim-32-1, fee $0.0236364399) |
| 1791283200000 | 32 | Orange | REFLEX | allow | order close-long 0.218 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.218 @ 2217.780 (sim-32-2, fee $0.1667992338) |

### repeated-orange-cooldown [synthetic]

- ticks: 19
- actions: 2
- baseline loss: $340 USD; sentinel loss: $202.7406710103 USD; saved: $137.2593289897 USD; sim fees: $0.2209310103 USD; scenario notional: $1360 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240060000 | 32 | Orange | REFLEX | allow | order close-short 0.125 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.125 @ 2847.845 (sim-32-1, fee $0.122813315625) |
| 1791240660000 | 32 | Orange | REFLEX | allow | order close-short 0.093 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.093 @ 3058.055 (sim-32-2, fee $0.098117694675) |

### slow-bleed-15 [synthetic]

- ticks: 13
- actions: 1
- baseline loss: $306 USD; sentinel loss: $269.23164361832 USD; saved: $36.76835638168 USD; sim fees: $0.16189961832 USD; scenario notional: $2040 USD
- liquidations: baseline 0; sentinel 0; avoided 0
- false positive reduces: 1; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791268800000 | 32 | Orange | REFLEX | allow | order close-long 0.187 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.187 @ 2509.488 (sim-32-1, fee $0.16189961832) |

### stale-feed-outage [synthetic]

- ticks: 39
- actions: 2
- baseline loss: $326.4 USD; sentinel loss: $114.99235539 USD; saved: $211.40764461 USD; sim fees: $0.25435539 USD; scenario notional: $1632 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- note: event note: simulated market-data outage; reduces resume after the window

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240090000 | 32 | Orange | REFLEX | alert: stale data (30s) at Orange tier: reduce disabled, manual attention required |
| 1791240690000 | 32 | Red | REFLEX | allow | order close-long 0.3 (slippage cap 10 bps) | red tier: first-breach reduce: filled 0.300 @ 2457.540 (sim-32-1, fee $0.25435539) |

### whale-vs-caps [synthetic]

- ticks: 25
- actions: 2
- baseline loss: $2720 USD; sentinel loss: $748.2360579403 USD; saved: $1971.7639420597 USD; sim fees: $1.9597979403 USD; scenario notional: $13600 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240900000 | 32 | Orange | REFLEX | allow | order close-long 1.25 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 1.250 @ 2687.310 (sim-32-1, fee $1.1589024375) |
| 1791247200000 | 32 | Orange | REFLEX | allow | order close-long 0.937 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.937 @ 2477.520 (sim-32-2, fee $0.8008955028) |

### wick-both [synthetic]

- ticks: 20
- actions: 4
- baseline loss: $299.2 USD; sentinel loss: $31.88430115505 USD; saved: $267.31569884495 USD; sim fees: $0.20359115505 USD; scenario notional: $1496 USD
- liquidations: baseline 1; sentinel 0; avoided 1
- false positive reduces: 0; policy violations: 0
- notes: none

| ts_ms | market | tier | source | detail |
|---|---|---|---|---|
| 1791240120000 | 32 | Orange | REFLEX | allow | order close-long 0.137 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.137 @ 2557.440 (sim-32-1, fee $0.1208774016) |
| 1791240120000 | 32 | Orange | STRATEGY | HOLD: post-review of the first-breach trim: keep the reduced position |
| 1791240720000 | 32 | Orange | REFLEX | allow | order close-long 0.103 (slippage cap 10 bps) | orange tier: de-risking reduce: filled 0.103 @ 2327.670 (sim-32-2, fee $0.08271375345) |
| 1791240720000 | 32 | Orange | STRATEGY | HOLD: post-review at the cooldown boundary: wick regime, no further change |

