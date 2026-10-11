# Why Sentinel puts Qwen 3.8 Max on the strategy path — and keeps it off the reflex path

> Sentinel is a non-custodial risk guardian for isolated-margin perpetual futures on [Perpl](https://app.perpl.xyz) (Monad). Every decision it makes is hash-chained, anchored on-chain, and independently verifiable. This article is about one layer of that system: the strategy brain, and why Alibaba's Qwen is the primary model in it.

## The problem a perp trader actually has

Isolated margin does not work the way most traders assume. In cross margin, one account balance absorbs losses across positions. In isolated margin — the model Perpl uses — each position carries its own collateral, and free balance elsewhere in the account does **not** protect it. A single position can march toward liquidation while the trader sleeps, and the liquidation price is not even handed to you by the gateway; it has to be derived from first principles and cross-checked.

Two facts follow. First, the risk is per-position and continuous, not per-account and occasional. Second, a liquidation does not need a crash: a gap through the liquidation level settles the position before a human can click anything.

So a guardian has to do two very different jobs:

1. **React in microseconds** when a threshold is breached.
2. **Think about the situation** when the picture is ambiguous.

Conflating those two jobs is how automated guardians get people hurt. Sentinel separates them structurally.

## Two brains, two very different contracts

Sentinel runs a **deterministic reflex engine** and an **LLM strategy brain**, and the boundary between them is the whole design.

**The reflex path contains no LLM at all.** It evaluates every position on every tick, derives distance-to-liquidation from isolated-margin first principles, classifies the position into Green / Yellow / Orange / Red tiers, and emits reduce-only intents. Measured cost of classification: **0.271 µs per iteration** (10,000 iterations in 2.707756 ms, 2,200 intents — `docs/evidence/p04-skill-verification.txt`). It does not call a network, does not read a clock, and contains no `unsafe` code.

**The strategy brain is consulted only at transitions** — when a position enters Yellow or Orange, after a reflex action, and on a periodic portfolio review. It proposes; the deterministic policy engine disposes. Reducing, closing, or adding collateral to the isolated position are the only admissible directions. The model cannot increase exposure, because the schema it must satisfy does not contain an action that increases exposure.

That is the key idea: **Qwen is not trusted to be safe. It is constrained to be useful.**

## What Qwen actually does

At a Yellow or Orange transition, Sentinel assembles a prompt from:

- the live account and position snapshot (size, entry, collateral, mark, margin health, distance-to-liquidation),
- the policy envelope and caps,
- recent reflex actions,
- and a smart-money context block bought per-call from Nansen over x402 (USDC on Monad, no API key).

Qwen 3.8 Max then returns a strict JSON decision. The wiring is deliberately conservative:

**1. A versioned, grounded system prompt.** `PROMPT_VERSION v3.0` carries hard grounding rules. The instruction to the model is not "be helpful"; it is:

```
Never recommend increasing exposure: reducing, closing, or adding collateral to the
isolated position are the only admissible directions.
Ground every claim in the user message: cite only numbers present there, and never
invent, round, or extrapolate one.
```

**2. `response_format: json_object`, with a budget sized for the thinking trace.** Qwen's reasoning preamble is useful to the model and useless to the parser, so the budget is sized generously and the trace is stripped.

**3. A tolerant parse pipeline: strip → extract → parse → validate.** The parser removes the thinking preamble, extracts the first balanced JSON object (not a greedy regex — balanced brace matching), deserializes with unknown fields ignored, then validates the decision against the allowed markets and the action schema. If validation fails, there is exactly **one repair nudge**, then it fails closed.

**4. A confidence floor.** A weak answer does not get a vote. Below the floor, the decision is downgraded to `ESCALATE` and routed to a human.

**5. A grounding check.** Every number in the model's stated reason must appear in the prompt it was given. A number that does not is a hallucinated number, and the decision is rejected.

## What the numbers say

The brain ships with an offline evaluation harness that runs against mock providers, so the scoreboard is reproducible with zero keys and zero network:

| Metric | Result |
|---|---|
| Action-class accuracy (core scenarios) | **12 / 12** |
| Schema validity | **14 / 14** |
| Prompt-injection schema validity | **2 / 2** |
| Grounding | **14 / 14** |

Source: `docs/evidence/p07-brain-eval-mock.txt`.

The two prompt-injection rows matter. The test corpus includes scenarios where the *symbol or note text itself* is hostile — the kind of thing that arrives through a symbol name or a note field and tries to steer the model. Structurally, the damage is bounded before the model is even asked: the reflex path has no LLM to inject into, and whatever the model returns passes the same policy gate as everything else. But the harness still checks that a hostile payload cannot flip the schema or the grounding check. It does not.

## Where the value actually is

It is tempting to frame an LLM in a trading system as "the AI that trades". That framing is both unsafe and wrong.

Qwen's value in Sentinel is **judgment at the transitions** — the moments where a deterministic rule is technically satisfied but the situation is not obviously right. A threshold says Orange; the position is large, the funding is unusual, the account is thin. Do you trim 25%, trim 50%, add collateral, or escalate to a human? A hard-coded table has to guess a policy for every combination in advance. Qwen can read the whole situation and propose, with a reason, in a form the machine can check.

The reason is logged. The confidence is logged. The provider that answered is logged. And the decision it proposed is either executed, modified, or denied by a deterministic gate that has no model in it.

That is the trade: **the model supplies judgment; the system supplies the guarantee.** You get the flexibility of an LLM where flexibility is genuinely useful, and you keep the auditability of a rule engine where auditability is non-negotiable.

## Honest status

The first keyed live call against the DashScope endpoint is **pending a live Qwen API key** (`STUB-01` in `docs/STUBS.md`); the probe script is written and the offline evaluation above is fully green. Everything claimed in this article is reproducible from the repository without credentials. That is the standard Sentinel holds itself to: *every number traceable to a file.*

## Repository

**github.com/JuanLunaIA/Sentinel** — the reflex engine, both brain providers, the policy gate, the hash-chained audit journal, the on-chain anchor, the Chainlink CRE dead-man's switch, and the backtester all ship in one Rust workspace.

---

*Sentinel is built by Juan Luna. Licensed Apache-2.0.*