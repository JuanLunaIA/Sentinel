#!/usr/bin/env node
/**
 * Independent EIP-712 oracle for the Sentinel P09 verifier (SPEC-P09 §7/§8).
 *
 * Computes the EIP-3009 `TransferWithAuthorization` digest + signature for the
 * frozen verifier vector below with ethers (installed from npm — NOT the Rust
 * implementation). `crates/sentinel/tests/nansen_adversarial.rs` pins the
 * printed values into `ORACLE_SIGNER_ADDRESS` / `ORACLE_DIGEST` /
 * `ORACLE_SIGNATURE`.
 *
 * Exact run command (ethers 6.17.0 is pre-installed in the scratch dir):
 *   cp tests/fixtures/nansen/eip712_oracle.js ~/.hermes/cache/scratch/oracle/
 *   cd ~/.hermes/cache/scratch/oracle && node eip712_oracle.js
 *
 * Output (stdout): one JSON line {signerAddress, digest, signature}.
 */
"use strict";

const { ethers } = require("ethers");

// Frozen vector — same shape as SPEC-P09 §7 (verifier-owned values; the
// private key is a well-known public test key, never funded).
const VECTOR = {
  key: "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
  domain: {
    name: "USDC", // rail `extra.name`
    version: "2", // rail `extra.version`
    chainId: 143n, // rail `network` eip155:143
    verifyingContract: "0x754704Bc059F8C67012fEd69BC8A327a5aafb603", // rail `asset`
  },
  types: {
    TransferWithAuthorization: [
      { name: "from", type: "address" },
      { name: "to", type: "address" },
      { name: "value", type: "uint256" },
      { name: "validAfter", type: "uint256" },
      { name: "validBefore", type: "uint256" },
      { name: "nonce", type: "bytes32" },
    ],
  },
  message: {
    from: "0x1111111111111111111111111111111111111111",
    to: "0x2222222222222222222222222222222222222222",
    value: 10000n,
    validAfter: 1740672089n,
    validBefore: 1740672389n,
    nonce: "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
  },
};

async function main() {
  const wallet = new ethers.Wallet(VECTOR.key);
  const signature = await wallet.signTypedData(
    VECTOR.domain,
    VECTOR.types,
    VECTOR.message,
  );
  const digest = ethers.TypedDataEncoder.hash(
    VECTOR.domain,
    VECTOR.types,
    VECTOR.message,
  );
  process.stdout.write(
    JSON.stringify(
      { signerAddress: wallet.address, digest, signature },
      null,
      2,
    ) + "\n",
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
