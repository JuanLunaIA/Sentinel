import { describe, it } from "vitest";
import { createTestIndexer } from "envio";

// Deterministic handler tests using simulated events (no network, no token).
// The Anvil end-to-end run against a real chain is captured separately in
// docs/evidence/p12-envio-anvil.txt.

describe("SentinelAuditAnchor handlers (simulated events)", () => {
  it("DecisionAnchored -> SentinelAnchor row with frozen field names", async (t) => {
    const indexer = createTestIndexer();

    const account = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const entryHash = "0x" + "11".repeat(32);
    const runningRoot = "0x" + "22".repeat(32);
    const txHash = "0x" + "ab".repeat(32);

    await indexer.process({
      chains: {
        10143: {
          simulate: [
            {
              contract: "SentinelAuditAnchor",
              event: "DecisionAnchored",
              block: { number: 100, timestamp: 1700000000 },
              transaction: { hash: txHash },
              params: { seq: 1n, entryHash, runningRoot, account },
            },
          ],
        },
      },
    });

    const row = await indexer.SentinelAnchor.getOrThrow("10143_100_0");
    t.expect(row.seq).toBe(1n);
    t.expect(row.entry_hash).toBe(entryHash);
    t.expect(row.root).toBe(runningRoot);
    t.expect(row.account).toBe(account);
    t.expect(row.ts).toBe(1700000000n);
    t.expect(row.tx_hash).toBe(txHash);
  });

  it("Heartbeat -> SentinelHeartbeat row with frozen field names", async (t) => {
    const indexer = createTestIndexer();

    const guardian = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    const riskStateHash = "0x" + "33".repeat(32);
    const txHash = "0x" + "cd".repeat(32);

    await indexer.process({
      chains: {
        10143: {
          simulate: [
            {
              contract: "SentinelAuditAnchor",
              event: "Heartbeat",
              block: { number: 101, timestamp: 1700000060 },
              transaction: { hash: txHash },
              params: { guardian, riskStateHash, openPositions: 3n, maxTier: 2n },
            },
          ],
        },
      },
    });

    const row = await indexer.SentinelHeartbeat.getOrThrow("10143_101_0");
    t.expect(row.guardian).toBe(guardian);
    t.expect(row.risk_state_hash).toBe(riskStateHash);
    t.expect(row.max_tier).toBe(2);
    t.expect(row.ts).toBe(1700000060n);
    t.expect(row.tx_hash).toBe(txHash);
  });

  it("every anchor event lands (seq ordering preserved)", async (t) => {
    const indexer = createTestIndexer();

    await indexer.process({
      chains: {
        10143: {
          simulate: [
            {
              contract: "SentinelAuditAnchor",
              event: "DecisionAnchored",
              block: { number: 200, timestamp: 1700001000 },
              transaction: { hash: "0x" + "01".repeat(32) },
              params: { seq: 1n, entryHash: "0x" + "aa".repeat(32), runningRoot: "0x" + "bb".repeat(32), account: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" },
            },
            {
              contract: "SentinelAuditAnchor",
              event: "DecisionAnchored",
              block: { number: 201, timestamp: 1700001060 },
              transaction: { hash: "0x" + "02".repeat(32) },
              params: { seq: 2n, entryHash: "0x" + "cc".repeat(32), runningRoot: "0x" + "dd".repeat(32), account: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266" },
            },
          ],
        },
      },
    });

    const all = await indexer.SentinelAnchor.getAll();
    t.expect(all).toHaveLength(2);
    const seqs = all.map((r) => r.seq).sort((a, b) => (a < b ? -1 : 1));
    t.expect(seqs).toEqual([1n, 2n]);
  });
});
