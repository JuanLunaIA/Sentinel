/**
 * Sentinel indexer — event handlers (P12).
 *
 * Registered events:
 *   SentinelAuditAnchor.DecisionAnchored -> SentinelAnchor
 *   SentinelAuditAnchor.Heartbeat        -> SentinelHeartbeat
 *
 * Both come from OUR ABI (contracts/src/SentinelAuditAnchor.sol). Exchange
 * events (PerpFill / Liquidation / AccountSnapshotDay) are intentionally NOT
 * handled here yet — the Perpl exchange ABI is unresolved (see README,
 * "Perpl-event indexing: roadmap"). The entities are declared in
 * schema.graphql so the tables and GraphQL surface exist as soon as the ABI
 * lands and a handler is added below.
 */
import { indexer } from "envio";

// Block timestamp + tx hash are the two extra fields our entities need.
// Everything else stays out of the event payload (see `indexer-transactions`).
const anchorEventFields = {
  block: ["timestamp"],
  transaction: ["hash"],
} as const;

indexer.onEvent(
  {
    contract: "SentinelAuditAnchor",
    event: "DecisionAnchored",
    fields: anchorEventFields,
  },
  async ({ event, context }) => {
    context.SentinelAnchor.set({
      // `disable_default_cross_chain: true` -> rows are keyed by (id, chainId);
      // prefixing with the chain id keeps ids unambiguous across chains anyway.
      id: `${event.chainId}_${event.block.number}_${event.logIndex}`,
      seq: event.params.seq,
      entry_hash: event.params.entryHash,
      root: event.params.runningRoot,
      account: event.params.account,
      ts: BigInt(event.block.timestamp),
      tx_hash: event.transaction.hash,
    });
  },
);

indexer.onEvent(
  {
    contract: "SentinelAuditAnchor",
    event: "Heartbeat",
    fields: anchorEventFields,
  },
  async ({ event, context }) => {
    context.SentinelHeartbeat.set({
      id: `${event.chainId}_${event.block.number}_${event.logIndex}`,
      guardian: event.params.guardian,
      risk_state_hash: event.params.riskStateHash,
      max_tier: Number(event.params.maxTier),
      ts: BigInt(event.block.timestamp),
      tx_hash: event.transaction.hash,
    });
  },
);
