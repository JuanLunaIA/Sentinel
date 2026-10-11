# A guardian that can only think with one vendor is a single point of failure

> Sentinel is a non-custodial risk guardian for isolated-margin perpetual futures on [Perpl](https://app.perpl.xyz) (Monad). It runs two providers on its strategy path: Alibaba's **Qwen** as primary, Moonshot's **Kimi** as fallback. This article is about the fallback — why it exists, how it is wired, and how the failover is *proved* rather than claimed.

## Why a fallback at all

Sentinel's strategy brain is consulted at risk transitions: when a position slips into Yellow or Orange, after a reflex action fires, and on periodic reviews. The reflex engine is deterministic and vendor-free, so a provider outage never blocks a protective reduce. But it does mean the guardian loses its judgement layer — and that is exactly the moment you would like judgement.

Consider the failure modes of a single-vendor design:

- **The provider returns 5xx** for a window. The guardian has no judgement during that window.
- **The provider returns a 200 with a malformed body** — a truncated JSON, a changed field name, a new schema. The parser fails and the consult is lost.
- **The provider rate-limits** under load, which is precisely when the market is busy.
- **The provider has a regional incident.** One vendor, one region, one point of failure.

"Retry with backoff" handles the first case and none of the others. The correct answer for a system whose job is to protect money is **two independent providers, with per-provider health, and a recorded answer about which one actually spoke.**

## The chain: Qwen → Kimi

Every consult goes through the same engine regardless of provider. The provider is an implementation of one trait:

```rust
impl Provider for KimiProvider {
    async fn complete(&self, system: &str, user: &str) -> Result<RawCompletion> {
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": Self::MAX_TOKENS,
            "temperature": Self::TEMPERATURE,
        });
        // … POST to the Moonshot chat-completions endpoint
    }
    fn name(&self) -> &'static str { "kimi" }
}
```

Because Kimi speaks the same OpenAI-compatible shape, the *entire* downstream stack is reused unchanged: the same versioned system prompt, the same tolerant JSON parser (strip thinking → balanced-object extraction → deserialize → validate), the same single repair nudge, the same grounding check, the same confidence floor. The fallback is not a second, weaker code path that only runs when nobody is looking. It is the same path with a different socket on the end of it.

One deliberate difference: the Kimi request does **not** set `response_format: json_object`. Where the primary can be asked for JSON mode, the fallback cannot assume it exists — so the parser that handles Kimi's output is the same tolerant one that handles Qwen's thinking trace. Robustness is the default, not a special case.

## Circuit breakers, per slot

A fallback that gets hammered while the primary is down becomes the new outage. Sentinel puts a **circuit breaker on each provider slot** — closed / open / half-open — so a provider that is failing is quarantined instead of retried into the ground. When the primary's breaker is open, consults go straight to the fallback without paying the primary's timeout first. When both are open, the consult reports *both* error classes rather than hiding one behind the other.

Failover is not retry. Retry is "the same thing, again, immediately". Failover is "a *different* thing, now".

## Failover is proved, not asserted

The claim "we have a fallback" is worth nothing unless you can force the primary to die on demand. Sentinel ships a forced-failure hook, and the evaluation harness runs the *entire* brain scoreboard with the primary force-failed:

```
FORCE_PROVIDER_FAIL=qwen cargo run --bin brain_eval -- --mock
```

Result — **14 / 14 PASS**, with `provider=kimi` on **every** row:

| Row class | Result with Qwen force-failed |
|---|---|
| Core action-class accuracy | 12 / 12 |
| Schema validity | 14 / 14 |
| Prompt-injection rows (#13–#14) | **PASS**, provider=kimi |
| Grounding | 14 / 14 |

Source: `docs/evidence/p08-failover.txt`.

Two details in that table are the ones that matter. First, the injection rows pass on the *fallback* — the safety properties of the brain (schema validity, grounding) are properties of the engine, not of one vendor. Second, every row records `provider=kimi`, which means the failover is not a code path that exists in a diagram; it executed, fourteen times, including on the adversarial rows.

## Auditable by construction

The consult result carries the provider that answered, and that value is written into the hash-chained audit journal alongside the decision. So the question "did this guardian actually answer on the primary, or was it silently degrading for six hours?" has an answer you can query after the fact, not a log line you have to trust.

That matters more than it sounds. A guardian that silently degrades is a guardian you cannot reason about. A guardian that *records* which brain answered every time is one you can audit against its own claims.

## Honest status

The failover machinery is fully implemented and tested offline. The first keyed live call against Moonshot is **pending a live Kimi API key** (`STUB-02` in `docs/STUBS.md`), at which point the model string is confirmed against the live endpoint. The offline proof above requires no credentials and is reproducible from the repository.

## The general principle

Vendor redundancy in a safety-critical agent is not a nice-to-have. It is the difference between "our guardian reasons about your position" and "our guardian reasons about your position *when our vendor is up*". Sentinel's answer is a second, independent brain behind the same contract, with the health of each one measured, and the failover demonstrated by killing the primary on purpose.

**github.com/JuanLunaIA/Sentinel** — reflex engine, dual-brain strategy, policy gate, hash-chained audit journal, on-chain anchor and dead-man's switch, in one Rust workspace.

---

*Sentinel is built by Juan Luna. Licensed Apache-2.0.*