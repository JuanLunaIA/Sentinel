# tests/fixtures/p16 — verifier fixtures (SPEC-P16 §3, agent M)

| file | purpose |
|------|---------|
| `redaction-sample.log` | Synthetic sentinel log exercising the redaction vocabulary the secret-scan must tolerate (markers `[REDACTED]`, `<redacted>`, `***`, `(redacted)`), the exempt well-known anvil dev key #0 in a `--private-key` context, and hash-context 64-hex values that must **not** be flagged. Scanned by `secret_scan_*` tests in `crates/sentinel/tests/p16_adversarial.rs`. |

Notes:
- No real secret values are stored here; everything secret-shaped is either a
  documented-public dev key, a redaction marker, or a hash.
- The scanner also generates a per-run sample (tempdir) rendering the current
  non-placeholder `.env` secret values as redacted forms, and proves non-vacuity
  with an in-memory dirty twin (violations must be detected).
