#!/usr/bin/env python3
"""Independent HMAC-SHA256 oracle for SPEC-P14 §5 (breaker armed HTTP surface).

Written from the spec text only — no knowledge of the breaker implementation.

Usage:
    hmac_oracle.py vector
        Re-derive the SPEC §5 test vector from the exact body bytes printed in
        the spec and check it against the frozen expected digest.

    hmac_oracle.py sign <secret-hex> <body-file>
        Print `sha256=<hex>` for HMAC_SHA256(secret, body-bytes) where the body
        is read byte-exact from <body-file> (use `-` for stdin).

    hmac_oracle.py sign-arg <secret-hex> <body>
        Same, with the body passed as an argv string (encoded UTF-8 byte-exact).

Exit codes: 0 on success, 1 on mismatch/usage error.
"""

import hashlib
import hmac
import sys

# --- SPEC-P14 §5 frozen vector -------------------------------------------
# secret b"spec-test-secret"; body exactly as printed on the `body = ...` line.
SPEC_SECRET = b"spec-test-secret"
SPEC_BODY = (
    b'{"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",'
    b'"reason":"spec-vector","requested_at_ms":1791200000000}'
)
SPEC_SIG = "af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04"


def hmac_sha256_hex(secret: bytes, body: bytes) -> str:
    """HMAC_SHA256(secret, body) as lowercase hex — the §5 signature rule."""
    return hmac.new(secret, body, hashlib.sha256).hexdigest()


def main(argv: list[str]) -> int:
    cmd = argv[1] if len(argv) > 1 else "vector"
    if cmd == "vector":
        got = hmac_sha256_hex(SPEC_SECRET, SPEC_BODY)
        ok = hmac.compare_digest(got, SPEC_SIG)
        print(f"body_bytes_utf8 = {SPEC_BODY.decode()}")
        print(f"hmac_sha256     = {got}")
        print(f"spec_expected   = {SPEC_SIG}")
        print(f"MATCH           = {ok}")
        return 0 if ok else 1
    if cmd == "sign" and len(argv) == 4:
        secret = bytes.fromhex(argv[2])
        if argv[3] == "-":
            body = sys.stdin.buffer.read()
        else:
            with open(argv[3], "rb") as fh:
                body = fh.read()
        print(f"sha256={hmac_sha256_hex(secret, body)}")
        return 0
    if cmd == "sign-arg" and len(argv) == 4:
        secret = bytes.fromhex(argv[2])
        body = argv[3].encode("utf-8")
        print(f"sha256={hmac_sha256_hex(secret, body)}")
        return 0
    print(__doc__, file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
