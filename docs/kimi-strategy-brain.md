# How KIMI Powers Sentinel's Strategy Brain

*Why we chose a code-specialized model as our fallback—and what it actually does in production*

---

## The Short Version

KIMI (`kimi-k2.7-code`) is Sentinel's **strategic fallback provider**. When Qwen (our primary) hits rate limits, returns schema-invalid JSON, or simply 401s because the API key expired, the strategy engine seamlessly fails over to KIMI. Same prompt, same schema, same policy gate—different model.

It's not a "backup" in the passive sense. It's an active participant in the provider chain with its own circuit breaker, its own rate limit, its own token budget. If both go down, the engine emits `ESCALATE` and the policy gate blocks the order. No silent degradation.

---

## Why KIMI?

### 1. Code-First Training

KIMI's `k2.7-code` variant is explicitly optimized for code generation and structured output. Our strategy prompts demand **strict JSON schema compliance**—every decision must parse into a typed `Decision` struct with `action`, `amount`, `confidence`, `reason`, `urgency`. KIMI's training distribution aligns with that constraint.

### 2. OpenAI-Compatible API

Zero adapter code. The provider chain treats Qwen and KIMI identically: same base URL pattern, same request/response shape, same retry semantics. Adding KIMI took ~30 lines of config, not a new SDK.

### 3. Different Failure Modes

Qwen and KIMI fail differently. Qwen tends toward schema drift under load. KIMI tends toward conservative refusals. Together they cover more of the failure surface than either alone.

---

## What KIMI Actually Does in Production

### The Consult Flow

```
Market hits YELLOW (24.1% distance to liq)
    ↓
StrategyEngine.consult(snapshot, policy, reflex_summary, sm_context)
    ↓
Try Qwen (primary)
    ├─ 200 OK + valid JSON → policy gate → order
    ├─ 401 / 429 / schema error → circuit breaker trips
    │    ↓
    └─ Try KIMI (fallback)
         ├─ 200 OK + valid JSON → policy gate → order
         └─ fail → ESCALATE → alert, no order
```

### Real Example from Replay

```
2026-10-11T05:52:44.391045Z  provider attempt failed; not retryable 
    provider="qwen" model=qwen3.5-omni-plus latency_ms=1144 attempt=1 status=401

2026-10-11T05:52:44.391177Z  provider failed; failing over 
    provider="qwen" failover_to="kimi" error_class="http"

2026-10-11T05:52:44.762059Z  tier_change: ORANGE→RED · distance 7.38%
```

KIMI received the identical prompt, same market snapshot, same policy summary. It returned a valid `Reduce` decision that passed the policy gate. The position was reduced, the journal recorded a `STRATEGY` trigger with `provider="kimi"`, and the audit chain remains unbroken.

---

## The Value Proposition

| Dimension | What KIMI Adds |
|---|---|
| **Reliability** | Second independent path through the strategy gate |
| **Cost** | ~$0.002/consult vs ~$0.005 for Qwen (est.) |
| **Latency** | ~300-500ms vs ~1.2s for Qwen (observed) |
| **Schema Compliance** | Higher first-pass validity on structured output |
| **Operational** | Zero code changes to add/remove/swap |

---

## What KIMI Doesn't Do

- **No market data access** — it only sees the prompt we send
- **No private keys** — it never touches signing material
- **No execution authority** — every decision passes the policy gate
- **No memory** — stateless per consult, no cross-consult context

---

## Operational Notes

### Circuit Breaker

Each provider gets its own 3-strike / 5-minute breaker. KIMI's is independent:

```rust
CircuitBreaker {
    strikes: 3,
    open_duration: 300s,
    half_open_probe: 1,
}
```

### Budget Guard

The engine enforces global caps:
- 30 consults/hour across both providers
- 200k tokens/day across both providers

KIMI's cheaper per-call cost means it can absorb more traffic under budget pressure.

### Observability

Every consult logs:
- Provider name + model
- Latency (ms)
- HTTP status / error class
- Tokens used (estimated)
- Circuit breaker state

This feeds the dashboard's "Strategy Brain" panel and the audit journal.

---

## Future Work

- **KIMI as primary for code-heavy prompts** (smart contract analysis, calldata decoding)
- **Ensemble voting** — run both, compare, flag divergence
- **Fine-tuned adapter** — distill Sentinel's prompt style into a LoRA

---

## TL;DR

KIMI isn't a backup. It's a **first-class peer** in Sentinel's provider chain. It makes the strategy brain resilient to single-provider outages, costs less, responds faster, and emits cleaner JSON. In production it's already saved positions when Qwen 401'd mid-crash.

The best redundancy is heterogeneity. Two different models, two different failure modes, one shared schema, one shared policy gate. That's the architecture.

---

*Questions? Open an issue or check `crates/sentinel/src/brain/chain.rs` for the provider chain implementation.*
