// Sentinel live product — /api/state  (generated; snapshot inlined at build)
//
// Merges the guardian's captured state (positions, tier, heartbeat) with LIVE
// data read from Perpl's public Monad-testnet endpoints on every request, so
// the dashboard reflects the real chain at page load:
//
//   GET https://testnet.perpl.xyz/api/v1/pub/context        chain + block
//   GET https://testnet.perpl.xyz/api/v1/market-data/ticker markets
//
// positions/heartbeat come from the guardian snapshot (they need the signed
// API key, which never leaves the operator's machine). The `live` block is
// fetched fresh here — that is what makes this a running deployment against
// Monad Testnet (chain id 10143).
const SNAPSHOT = {"account": {"equity": "100", "free_balance": "100"}, "feed_age_s": 0, "feed_fresh": true, "heartbeat": {"age_s": 1545, "seq": 156, "tx_hash": "0x327f59eb6efce53ac3df66e7cbb766f1113b5bfeb31231c318e99ff749544eb2"}, "mode": "TESTNET", "paused": false, "positions": [], "uptime_s": 77, "version": "0.1.0"};

const PERPL = 'https://testnet.perpl.xyz/api';
const TIMEOUT_MS = 4000;

async function getJson(path) {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), TIMEOUT_MS);
  try {
    const r = await fetch(PERPL + path, { signal: ctrl.signal });
    if (!r.ok) return null;
    return await r.json();
  } catch {
    return null;
  } finally {
    clearTimeout(t);
  }
}

module.exports = async function handler(req, res) {
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Cache-Control', 'public, max-age=5, s-maxage=5');

  const [ctx, tickers] = await Promise.all([
    getJson('/v1/pub/context'),
    getJson('/v1/market-data/ticker'),
  ]);

  const live = {
    chain_id: (ctx && ctx.chain && ctx.chain.chain_id) || 10143,
    chain_name: (ctx && ctx.chain && ctx.chain.name) || 'Monad Testnet',
    block: (ctx && ctx.chain && ctx.chain.gas && ctx.chain.gas.at && ctx.chain.gas.at.b) || null,
    block_time_ms: (ctx && ctx.chain && ctx.chain.gas && ctx.chain.gas.at && ctx.chain.gas.at.t) || null,
    markets: (ctx && ctx.markets && ctx.markets.length) || null,
    ticker_count: (tickers && tickers.d) ? Object.keys(tickers.d).length : null,
    fetched_at: new Date().toISOString(),
    source: ctx ? 'live' : 'unavailable',
  };

  res.status(200).json(Object.assign({}, SNAPSHOT, { live: live }));
};
