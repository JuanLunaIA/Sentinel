# How Sentinel thinks

This is the plain-language version. No jargon, no diagrams. If you want the
engineering detail instead, read [architecture.md](architecture.md).

## In one sentence

Sentinel watches your open positions on Perpl and reduces them, in stages, when
they get too close to liquidation.

## The problem it solves

Perpl runs on isolated margin. That means each position carries its own
collateral. Your free balance, the cash sitting elsewhere in the account, does
not protect it. That is worth saying twice, because it is the part people miss.

So you can have money in your account and still lose a position, because that
money was never attached to it.

And it does not take a crash. A single price gap through the liquidation level
settles the position before a human can click anything.

There is one more wrinkle: the exchange does not tell you the liquidation price.
You have to work it out yourself from the market's margin rules. Most people
never do, which means most people do not know how much room they actually have.

## What Sentinel measures

One thing: **how far the price can move before this specific position gets
liquidated**, expressed as a percentage.

It is not a guess and it is not a fixed number somebody typed into a config
file. Sentinel derives it from that market's own margin rules (maintenance
margin, entry price, size, collateral) and recomputes it every time the position
changes. If you add collateral, the number moves. If you get partially filled,
the number moves. It always reflects the position as it is right now.

This is also why Sentinel does not try to predict where the price is going. It
does not care. It cares about how much room is left.

## The four levels

| Level | Distance to liquidation | What Sentinel does |
|---|---|---|
| **Green** | 25% or more | Nothing. |
| **Yellow** | 15% to 25% | Nothing to the position. It asks the AI for a read on the situation. |
| **Orange** | 8% to 15% | Trims about a quarter of the position. |
| **Red** | Under 8% | Trims about half. If it breaches again, closes what remains. |

The stages are the point. A simple bot that fires "if the price hits X, close"
dumps your entire position the moment a line is crossed, even if the price
recovers a minute later. Sentinel takes some risk off at Orange, takes more off
at Red, and closes fully only if the position breaches a second time.

A position that recovers to Yellow or Green resets: the next breach starts the
stages over.

## Why the AI does not run the protection

Sentinel has two brains, and the split is deliberate.

**The reflex brain is deterministic.** It is the one that measures and cuts, and
it has no AI in it at all. It runs on every tick, in microseconds, with no
network call and nothing to wait for. This is the part that protects you.

**The strategy brain is the AI** (Qwen 3.8 Max, with Kimi as the fallback). It
gets asked for a read when a position enters the Yellow band and again after a
trim. It can suggest context and next steps. What it cannot do is execute
anything. Its output goes through the same policy gate as every other proposal,
and the reflex path does not depend on it in any way.

The practical consequence: the model can be slow, wrong, or completely offline,
and your position still gets protected. The protection never ran through it.

## Why you do not have to trust it

Any automated system that touches your money has to answer one question: how do
I know it did what it claims?

Sentinel's answer is a log you can check. Every decision goes into a journal
where each entry carries the hash of the entry before it. Change any entry and
the chain breaks, visibly. Batches of decisions get anchored on-chain on Monad,
so the record is not just sitting on some server that could quietly rewrite it.
There is a public indexer, and the dashboard has a verifier that walks the chain
entry by entry and tells you whether it is intact.

There is also a dead-man's switch that runs outside the main process. If
Sentinel stops signaling, the switch can act. It exists precisely because
Sentinel itself can fail.

## What it will never do

- Open a position.
- Add to a position.
- Increase your exposure in any way.

It can only reduce. That is not a setting you can accidentally flip. It is how
the system is built, and the test suite checks it.

## What it does not do

Worth being straight about:

- **It does not predict prices.** No model, no signal, no forecast. It measures
  your distance to liquidation and acts on that.
- **It does not guarantee you will not get liquidated.** If the price gaps
  through your liquidation level in one move, no reactive system can save that
  position. We tested it: the loss is roughly the same with or without Sentinel.
  The value is in the bars before the gap, not at the gap.
- **It is not audited by a third party yet.**
- **It runs on testnet today.** Live keys, funded wallets, and the on-venue
  steps are listed openly in the [README](../README.md), each with a local
  equivalent that has already been verified.

## How to see it working

Open <https://sentinel-live-amber.vercel.app>. That page reads Monad testnet
live on every load, so the chain panel and block number are real, not a mockup.

Then look at the backtest: 14 scenarios, where doing nothing ends in 11
liquidations and Sentinel ends in zero. Or the second suite, whose price paths
come from the real testnet tape instead of being drawn by hand. Both reports are
in the repo, including the scenarios where Sentinel costs a few dollars instead
of saving them. Those are in there too.

That last part is the honest version, and it is the one worth reading.
