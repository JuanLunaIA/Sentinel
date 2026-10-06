// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title SentinelAuditAnchor
/// @notice On-chain anchor for the Sentinel audit-journal hash chain (SPEC-P10 §4).
/// @dev Per-account replay guard. Every account owns an independent,
///      exactly-incrementing seq chain (1, 2, 3, …). `lastSeq` and `lastRoot`
///      are the only storage; `beat` is event-only and never writes state.
///      No constructor arguments.
contract SentinelAuditAnchor {
    /// @notice A decision entry (single or the tail of a batch) was anchored.
    event DecisionAnchored(uint64 seq, bytes32 entryHash, bytes32 runningRoot, address account);

    /// @notice Periodic risk heartbeat; informational, no state change.
    event Heartbeat(address guardian, bytes32 riskStateHash, uint32 openPositions, uint8 maxTier);

    /// @notice Caller's seq was not the next expected one.
    /// @param got  The seq supplied by the caller.
    /// @param want The seq the guard expected (`lastSeq[msg.sender] + 1`).
    error StaleSeq(uint64 got, uint64 want);

    /// @notice Highest anchored seq per account (replay guard).
    mapping(address => uint64) public lastSeq;

    /// @notice Running root of the anchored chain per account.
    mapping(address => bytes32) public lastRoot;

    /// @notice Anchor a single decision entry.
    /// @dev Reverts with `StaleSeq` unless `seq` is exactly one past the
    ///      caller's last anchored seq.
    /// @param seq         Journal seq being anchored; must equal lastSeq + 1.
    /// @param entryHash   Hash of the journal entry at `seq`.
    /// @param runningRoot Running root of the anchored chain after `seq`.
    function anchor(uint64 seq, bytes32 entryHash, bytes32 runningRoot) external {
        uint64 want = lastSeq[msg.sender] + 1;
        if (seq != want) {
            revert StaleSeq(seq, want);
        }
        lastSeq[msg.sender] = seq;
        lastRoot[msg.sender] = runningRoot;
        emit DecisionAnchored(seq, entryHash, runningRoot, msg.sender);
    }

    /// @notice Anchor the entry-hash tail `[fromSeq, fromSeq + len - 1]` as one batch.
    /// @dev Requires a non-empty array and `fromSeq == lastSeq + 1`; stores
    ///      `toSeq` and the last entry hash, and reports the batch merkle
    ///      `root` in the event.
    /// @param fromSeq     First journal seq in the batch; must equal lastSeq + 1.
    /// @param entryHashes Entry hashes in seq order (length > 0).
    /// @param root        Merkle root over `entryHashes` (event data only).
    function batchAnchor(uint64 fromSeq, bytes32[] calldata entryHashes, bytes32 root) external {
        uint256 len = entryHashes.length;
        require(len > 0);
        uint64 want = lastSeq[msg.sender] + 1;
        if (fromSeq != want) {
            revert StaleSeq(fromSeq, want);
        }
        // Safe: `entryHashes.length` is bounded by calldata size, far below 2^64.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint64 toSeq = fromSeq + uint64(len) - 1;
        bytes32 lastHash = entryHashes[len - 1];
        lastSeq[msg.sender] = toSeq;
        lastRoot[msg.sender] = lastHash;
        emit DecisionAnchored(toSeq, lastHash, root, msg.sender);
    }

    /// @notice Post a risk heartbeat; emits only, never writes storage.
    /// @param riskStateHash Hash of the canonical risk-state summary.
    /// @param openPositions Open positions at heartbeat time.
    /// @param maxTier       Highest risk tier in effect.
    function beat(bytes32 riskStateHash, uint32 openPositions, uint8 maxTier) external {
        emit Heartbeat(msg.sender, riskStateHash, openPositions, maxTier);
    }
}
