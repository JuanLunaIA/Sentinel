#!/usr/bin/env python3
"""Independent SHA-256 chain oracle for P10 journals (SPEC-P10 §2).

Usage: sha256_oracle.py <journal-file> [partsets-json]

Reads the journal's written canonical JSONL lines and recomputes every
entry_hash from the raw bytes — no Rust code is involved:

    entry_hash = sha256( raw32(prev_hash) ++ canonical_json(entry minus
                 {prev_hash, entry_hash}) )
    canonical_json(v) = json.dumps(v, sort_keys=True, separators=(",", ":"))

and re-links the chain (each line's prev_hash must equal the previous line's
recomputed digest; the first line must start at the 64-zero genesis).

partsets-json: optional JSON array (one entry per journal line) of arrays of
input-part JSON strings; when present, each line's input_hash is recomputed
as sha256(concat(canonical_json(part_i))). A null entry skips the check.

Prints one report line per entry plus a final ORACLE_OK / ORACLE_FAIL;
exit status 0 on success, 1 on any mismatch.
"""
import hashlib
import json
import sys


def canon(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def main():
    path = sys.argv[1]
    partsets = json.loads(sys.argv[2]) if len(sys.argv) > 2 else None
    with open(path, encoding="utf-8") as handle:
        lines = [line.rstrip("\n") for line in handle if line.strip()]
    expected_prev = "0" * 64
    ok = True
    for idx, line in enumerate(lines):
        entry = json.loads(line)
        stored = entry["entry_hash"]
        prev = entry["prev_hash"]
        body = {k: v for k, v in entry.items() if k not in ("prev_hash", "entry_hash")}
        digest = hashlib.sha256(bytes.fromhex(prev) + canon(body).encode()).hexdigest()
        link = prev == expected_prev
        match = digest == stored
        input_match = True
        if partsets and idx < len(partsets) and partsets[idx] is not None:
            parts = partsets[idx]
            ih = hashlib.sha256(
                "".join(canon(json.loads(p)) for p in parts).encode()
            ).hexdigest()
            input_match = ih == entry.get("input_hash")
        ok = ok and link and match and input_match
        print(
            f"line={idx} seq={entry['seq']} link={link} entry_match={match} "
            f"input_match={input_match} recomputed={digest} stored={stored}"
        )
        expected_prev = digest
    print("ORACLE_OK" if ok else "ORACLE_FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
