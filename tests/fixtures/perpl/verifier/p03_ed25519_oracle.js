#!/usr/bin/env node
// P03 verifier oracle — run from the repository root:
//
//   $ node tests/fixtures/perpl/verifier/p03_ed25519_oracle.js
//
// Verifies (and prints) the Ed25519 signatures for the frozen SPEC.md §3.1
// REST-1/REST-2 and §3.2 WS-1 canonical strings, using ONLY Node's crypto.
// Seed: 0707070707070707070707070707070707070707070707070707070707070707
// PKCS#8 DER = 302e020100300506032b657004220420 || seed
//
// Transcript (node v26.7.0):
//   seed_hex  = 0707070707070707070707070707070707070707070707070707070707070707
//   sha256("")           = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
//   sha256({"d":[]})      = 088214f816e99a2f4aedb5323c1c2eaf8b8143df9424ec46759966ddd9b72dd3
//   byteLength({"d":[]})  = 8
//   nonce_decoded_bytes   = 16
//   --- REST-1
//   canonical = "143\nGET\n/v1/trading/fills?count=100\n1728000000000\nAAAAAAAAAAAAAAAAAAAAAA\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
//   sig_b64url = tg2unwMXTHrFEeR1NHsFwupj1iVLo69RYcPeLvYhHwt5pRCrTWa62GfbNNl5ozwv3i6vtHQ10Oj1qbTi2NtOBg
//   sig_bytes = 64  verify = true
//   --- REST-2
//   canonical = "10143\nPOST\n/v1/trading/orders\n1728000001000\nAAAAAAAAAAAAAAAAAAAAAA\n088214f816e99a2f4aedb5323c1c2eaf8b8143df9424ec46759966ddd9b72dd3"
//   sig_b64url = pbi1rC0vtS5Zbvooid41Mzq4ATq3mczW5kF3pcYzQuXymLl99g20mbJiVejoekJfMk7RvazhDfGKoeqxN-teAA
//   sig_bytes = 64  verify = true
//   --- WS-1
//   canonical = "10143\ntrading-ws-signin\n1728000000000\nAAAAAAAAAAAAAAAAAAAAAA"
//   sig_b64url = qrTi3fOt0EmW_IinRzbCc3UmeBADd1txA7BYLqy5KETUnrCUIzl5gYyp04JVA5huOUm_TvkNhYneWBIjdEDRCg
//   sig_bytes = 64  verify = true
'use strict';
const crypto = require('crypto');

const seedHex = '07'.repeat(32);
const seed = Buffer.from(seedHex, 'hex');
const der = Buffer.concat([
  Buffer.from('302e020100300506032b657004220420', 'hex'),
  seed,
]);
const key = crypto.createPrivateKey({ key: der, format: 'der', type: 'pkcs8' });
const pub = crypto.createPublicKey(key);

const sha256 = (s) => crypto.createHash('sha256').update(s).digest('hex');
const nonce = 'AAAAAAAAAAAAAAAAAAAAAA';
const body2 = '{"d":[]}';

console.log('seed_hex  =', seedHex);
console.log('sha256("")           =', sha256(''));
console.log('sha256({"d":[]})      =', sha256(body2));
console.log('byteLength({"d":[]})  =', Buffer.byteLength(body2));
console.log('nonce_decoded_bytes   =', Buffer.from(nonce, 'base64url').length);

const canonicals = {
  'REST-1': [
    '143', 'GET', '/v1/trading/fills?count=100', '1728000000000', nonce,
    sha256(''),
  ].join('\n'),
  'REST-2': [
    '10143', 'POST', '/v1/trading/orders', '1728000001000', nonce,
    sha256(body2),
  ].join('\n'),
  'WS-1': ['10143', 'trading-ws-signin', '1728000000000', nonce].join('\n'),
};

for (const [name, canonical] of Object.entries(canonicals)) {
  const sig = crypto.sign(null, Buffer.from(canonical, 'utf8'), key);
  const b64 = sig.toString('base64url');
  const ok = crypto.verify(null, Buffer.from(canonical, 'utf8'), pub, sig);
  console.log(`--- ${name}`);
  console.log(`canonical = ${JSON.stringify(canonical)}`);
  console.log(`sig_b64url = ${b64}`);
  console.log(`sig_bytes = ${sig.length}  verify = ${ok}`);
}
