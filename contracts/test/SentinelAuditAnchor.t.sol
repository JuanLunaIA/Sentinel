// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {SentinelAuditAnchor} from "../src/SentinelAuditAnchor.sol";

/// @dev Minimal hevm/anvil cheatcode surface used by this suite. Declared
///      locally so the project needs no external test-framework dependency.
interface Vm {
    function prank(address msgSender) external;

    function expectRevert() external;

    function expectRevert(bytes calldata revertData) external;

    function expectEmit(bool checkTopic1, bool checkTopic2, bool checkTopic3, bool checkData, address emitter)
        external;
}

/// @dev Guard tests for `SentinelAuditAnchor` (SPEC-P10 §4): seq replay guard
///      for both anchor paths, batch math, exact event args, heartbeat, and
///      per-account chain independence.
contract SentinelAuditAnchorTest {
    /// @dev The cheatcode address: address(uint160(uint256(keccak256("hevm cheat code")))).
    Vm internal constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    SentinelAuditAnchor internal audit;

    address internal constant ALICE = address(0xA11CE);
    address internal constant BOB = address(0xB0B);

    bytes32 internal constant H1 = keccak256("entry-1");
    bytes32 internal constant H2 = keccak256("entry-2");
    bytes32 internal constant H3 = keccak256("entry-3");
    bytes32 internal constant H4 = keccak256("entry-4");
    bytes32 internal constant R1 = keccak256("running-root-1");
    bytes32 internal constant R2 = keccak256("running-root-2");
    bytes32 internal constant R3 = keccak256("running-root-3");
    bytes32 internal constant RISK = keccak256("risk-state");

    function setUp() public {
        audit = new SentinelAuditAnchor();
    }

    // ---------------------------------------------------------------- anchor

    function test_anchorAcceptsSequentialSeqs() public {
        assert(audit.lastSeq(ALICE) == 0);
        assert(audit.lastRoot(ALICE) == bytes32(0));

        vm.expectEmit(true, true, true, true, address(audit));
        emit SentinelAuditAnchor.DecisionAnchored(1, H1, R1, ALICE);
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        assert(audit.lastSeq(ALICE) == 1);
        assert(audit.lastRoot(ALICE) == R1);

        vm.expectEmit(true, true, true, true, address(audit));
        emit SentinelAuditAnchor.DecisionAnchored(2, H2, R2, ALICE);
        vm.prank(ALICE);
        audit.anchor(2, H2, R2);

        assert(audit.lastSeq(ALICE) == 2);
        assert(audit.lastRoot(ALICE) == R2);
    }

    function test_anchorRevertsOnZeroSeq() public {
        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(0), uint64(1)));
        vm.prank(ALICE);
        audit.anchor(0, H1, R1);

        assert(audit.lastSeq(ALICE) == 0);
        assert(audit.lastRoot(ALICE) == bytes32(0));
    }

    function test_anchorRevertsOnSkippedSeq() public {
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(3), uint64(2)));
        vm.prank(ALICE);
        audit.anchor(3, H3, R3);

        assert(audit.lastSeq(ALICE) == 1);
        assert(audit.lastRoot(ALICE) == R1);
    }

    function test_anchorRevertsOnReplay() public {
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(1), uint64(2)));
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        assert(audit.lastSeq(ALICE) == 1);
        assert(audit.lastRoot(ALICE) == R1);
    }

    // ----------------------------------------------------------- batchAnchor

    function test_batchAnchorAnchorsTailAndUpdatesState() public {
        bytes32[] memory hashes = new bytes32[](3);
        hashes[0] = H1;
        hashes[1] = H2;
        hashes[2] = H3;

        // toSeq = fromSeq + len - 1 = 3; entryHash field = last hash; root in event.
        vm.expectEmit(true, true, true, true, address(audit));
        emit SentinelAuditAnchor.DecisionAnchored(3, H3, R1, ALICE);
        vm.prank(ALICE);
        audit.batchAnchor(1, hashes, R1);

        assert(audit.lastSeq(ALICE) == 3);
        assert(audit.lastRoot(ALICE) == H3);
    }

    function test_batchAnchorSingleEntryContinuesAfterAnchor() public {
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        bytes32[] memory hashes = new bytes32[](1);
        hashes[0] = H2;

        // len == 1 => toSeq == fromSeq.
        vm.expectEmit(true, true, true, true, address(audit));
        emit SentinelAuditAnchor.DecisionAnchored(2, H2, R2, ALICE);
        vm.prank(ALICE);
        audit.batchAnchor(2, hashes, R2);

        assert(audit.lastSeq(ALICE) == 2);
        assert(audit.lastRoot(ALICE) == H2);
    }

    function test_batchAnchorRevertsOnOffByOneFromSeq() public {
        bytes32[] memory hashes = new bytes32[](2);
        hashes[0] = H1;
        hashes[1] = H2;

        // Skips seq 1 entirely (fromSeq 2, want 1).
        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(2), uint64(1)));
        vm.prank(ALICE);
        audit.batchAnchor(2, hashes, R1);

        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        // One past the guard: want = lastSeq + 1 = 2, got 3.
        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(3), uint64(2)));
        vm.prank(ALICE);
        audit.batchAnchor(3, hashes, R1);

        assert(audit.lastSeq(ALICE) == 1);
        assert(audit.lastRoot(ALICE) == R1);
    }

    function test_batchAnchorRevertsOnEmptyArray() public {
        bytes32[] memory empty = new bytes32[](0);

        vm.expectRevert();
        vm.prank(ALICE);
        audit.batchAnchor(1, empty, R1);

        assert(audit.lastSeq(ALICE) == 0);
        assert(audit.lastRoot(ALICE) == bytes32(0));
    }

    // ------------------------------------------------------------------ beat

    function test_beatEmitsHeartbeatAndChangesNoStorage() public {
        // Seed ALICE so "no storage change" is observable on touched state.
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);

        vm.expectEmit(true, true, true, true, address(audit));
        emit SentinelAuditAnchor.Heartbeat(ALICE, RISK, uint32(7), uint8(3));
        vm.prank(ALICE);
        audit.beat(RISK, 7, 3);

        assert(audit.lastSeq(ALICE) == 1);
        assert(audit.lastRoot(ALICE) == R1);

        // A pristine account stays pristine after beating.
        vm.prank(BOB);
        audit.beat(RISK, 0, 0);

        assert(audit.lastSeq(BOB) == 0);
        assert(audit.lastRoot(BOB) == bytes32(0));
    }

    // ------------------------------------------------------- chain isolation

    function test_secondCallerHasIndependentSeqChain() public {
        vm.prank(ALICE);
        audit.anchor(1, H1, R1);
        vm.prank(ALICE);
        audit.anchor(2, H2, R2);

        // BOB starts from his own genesis: seq 1 is valid for BOB too.
        vm.prank(BOB);
        audit.anchor(1, H3, R3);

        assert(audit.lastSeq(ALICE) == 2);
        assert(audit.lastRoot(ALICE) == R2);
        assert(audit.lastSeq(BOB) == 1);
        assert(audit.lastRoot(BOB) == R3);

        // BOB's guard tracks BOB's chain: seq 3 is stale for him (want 2)...
        vm.expectRevert(abi.encodeWithSelector(SentinelAuditAnchor.StaleSeq.selector, uint64(3), uint64(2)));
        vm.prank(BOB);
        audit.anchor(3, H4, R3);

        // ...while seq 2 is exactly what BOB must anchor next.
        vm.prank(BOB);
        audit.anchor(2, H4, R2);

        assert(audit.lastSeq(BOB) == 2);
        assert(audit.lastSeq(ALICE) == 2);
        assert(audit.lastRoot(ALICE) == R2);
    }
}
