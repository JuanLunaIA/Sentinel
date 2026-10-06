# Sentinel — Telegram manual acceptance checklist (P11)

**Status: PENDING-TOKEN** (STUB-18). Every logic layer (parse, escaping,
handlers, approvals, policy overlay) is covered by offline tests; this
checklist validates the live bot surface once a real `TELOXIDE_TOKEN`
exists.

## Setup (once)

1. Create the bot with @BotFather → put `TELOXIDE_TOKEN` into `.env`.
2. `TELEGRAM_ALLOWED_USER_IDS=<your numeric id>` (send /start to a bot that
   echoes `chat.id`, or use @userinfobot).
3. `TELEGRAM_APPROVAL_CHAT_ID=<chat id>` (can be your DM).
4. Start: `cargo run --bin sentinel -- --mode dry-run` — the log must show
   the bot polling line; the daemon keeps running the replay/live pipeline.

## Checklist (run top to bottom; screenshot each ✅ into
## `docs/evidence/p11-<n>-<command>.png`)

| # | Action | Expected |
|---|---|---|
| 1 | `/start` | Onboarding card: what Sentinel is, mode badge, capabilities, safety notes (kill switch denies everything). |
| 2 | `/status` | Positions with size/entry/mark/distance%/tier emoji 🟢🟡🟠🔴, collateral, uPnL; free balance; feed freshness; mode. |
| 3 | `/risk` | A live consult decision: action, confidence, urgency, reason, provider, policy verdict — or an honest DEGRADED line when keys are placeholders. |
| 4 | `/audit 5` | Last 5 journal entries (seq, trigger, action, status) + pointer at `audit-verify` for the on-chain cross-check. |
| 5 | `/spend` | x402 ledger totals: calls this hour vs cap, cost today, last purchases. |
| 6 | `/policy` | Effective caps/thresholds incl. any overlay overrides. |
| 7 | `/policy set risk_hard_pct 9` | Confirmation + refreshed card; `data/policy.json` updated; daemon log shows the pipeline picking up the new version. |
| 8 | `/policy set risk_hard_pct 99` | Rejected with reason (cross-field hard < warn < soft). |
| 9 | `/mode` | Mode display + honest note that switching needs a restart. |
| 10 | `/pause` → type `PAUSE` | Kill switch on; reply confirms; next reflex evaluation logs a deny (intent denied by kill switch); `data/policy.json` shows `kill_switch: true`. |
| 11 | `/resume` | Kill switch off; pipeline resumes acting. |
| 12 | `/close 32 0.3` | Confirm keyboard with card (size/notional); `✅ Execute` → execution report in chat (dry-run fill) + journal entries (Trigger HUMAN, status human_approved). `❌ Cancel` on a second try → denied + journaled. |
| 13 | Approvals round-trip | With `REQUIRE_APPROVAL_ABOVE_USD` lowered, run a crash-replay that queues a NeedsApproval reflex decision → keyboard arrives → approve → executed + journaled; third case: let one expire (5 min) → EXPIRED journal entry + notification. |
| 14 | Unknown user | Second Telegram account sends `/status` → NO reply; daemon log shows the rejection. |

## Evidence to capture

- Screenshots per row (12–14 are the money shots).
- `docs/evidence/p11-approval-roundtrip.txt`: journal tail showing
  `human_approved` + `expired` outcomes after row 13.
- After row 10: `data/policy.json` content in the screenshot.
