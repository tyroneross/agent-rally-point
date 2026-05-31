// D4 tests — ResyncMachine state machine
import XCTest
@testable import Cockpit

final class ResyncMachineTests: XCTestCase {

    // MARK: - Helpers

    private func makeEvent(sessionId: String = "s1", seq: UInt64, kind: String = "message") -> Event {
        Event(
            sessionId: sessionId,
            seq: seq,
            sender: "agent",
            kind: kind,
            content: "content \(seq)",
            requiresUserInput: false,
            createdAt: "2026-05-31T00:00:00Z",
            metadata: [:]
        )
    }

    // MARK: - Snapshot ingestion

    func testApplySnapshotSetsEvents() {
        let machine = ResyncMachine()
        let events = [makeEvent(seq: 1), makeEvent(seq: 2), makeEvent(seq: 3)]
        machine.applySnapshot(sessionId: "s1", events: events, cursorSeq: 3)

        let stored = machine.events(for: "s1")
        XCTAssertEqual(stored.map(\.seq), [1, 2, 3])
        XCTAssertEqual(machine.cursor(for: "s1"), 3)
    }

    func testApplySnapshotSortsOutOfOrderEvents() {
        let machine = ResyncMachine()
        let events = [makeEvent(seq: 3), makeEvent(seq: 1), makeEvent(seq: 2)]
        machine.applySnapshot(sessionId: "s1", events: events, cursorSeq: 3)

        let stored = machine.events(for: "s1")
        XCTAssertEqual(stored.map(\.seq), [1, 2, 3])
    }

    func testApplySnapshotReplacesExistingEvents() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1)], cursorSeq: 1)
        // Reconnect snapshot replaces old state
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1), makeEvent(seq: 2)], cursorSeq: 2)

        let stored = machine.events(for: "s1")
        XCTAssertEqual(stored.count, 2)
    }

    // MARK: - Delta ingestion

    func testApplyDeltaInsertsNewEvent() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1)], cursorSeq: 1)
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 2))

        XCTAssertEqual(machine.events(for: "s1").map(\.seq), [1, 2])
        XCTAssertEqual(machine.cursor(for: "s1"), 2)
    }

    func testApplyDeltaDropsDuplicate() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1), makeEvent(seq: 2)], cursorSeq: 2)
        let inserted = machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 2))  // dupe

        XCTAssertFalse(inserted)
        XCTAssertEqual(machine.events(for: "s1").count, 2)  // no growth
    }

    func testApplyDeltaDropsAlreadySeenSeq() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 5)], cursorSeq: 5)
        // Server sends seq=3 in replay (already below cursor) — must be dropped
        let inserted = machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 3))

        XCTAssertFalse(inserted)
        XCTAssertEqual(machine.events(for: "s1").count, 1)
    }

    func testApplyDeltaOrdersInsertCorrectly() {
        // Deltas arrive in-order from the server (seq monotonically increases).
        // The machine sorts its list by seq at each insert, so the result is ordered.
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1)], cursorSeq: 1)
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 2))
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 3))

        XCTAssertEqual(machine.events(for: "s1").map(\.seq), [1, 2, 3])
    }

    // MARK: - Reconnect cursor (D4 key invariant)

    /// On reconnect, fromSeq must be the highest seq previously seen.
    func testFromSeqReturnsHighestSeenCursor() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 1), makeEvent(seq: 4)], cursorSeq: 4)
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 5))

        // Reconnect should ask for events after seq=5
        XCTAssertEqual(machine.fromSeq(for: "s1"), 5)
    }

    func testFromSeqIsZeroForNewSession() {
        let machine = ResyncMachine()
        XCTAssertEqual(machine.fromSeq(for: "never-opened"), 0)
    }

    // MARK: - No gaps, no dupes

    func testSnapshotThenDeltaNoGapsNoDupes() {
        let machine = ResyncMachine()
        // Server sends snapshot events 1..3, cursor=3
        let snapshotEvents = (1...3).map { makeEvent(seq: UInt64($0)) }
        machine.applySnapshot(sessionId: "s1", events: snapshotEvents, cursorSeq: 3)

        // After reconnect, server replays seq>2 (so 3 again) + live 4,5
        // The applyDelta should dedupe seq=3 and add 4,5
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 3))  // dupe
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 4))
        machine.applyDelta(sessionId: "s1", event: makeEvent(seq: 5))

        let seqs = machine.events(for: "s1").map(\.seq)
        XCTAssertEqual(seqs, [1, 2, 3, 4, 5])  // exactly once each, no dupes
    }

    // MARK: - Background handling (cursor preserved)

    func testBackgroundPreservesCursor() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(seq: 10)], cursorSeq: 10)
        machine.handleBackground(sessionId: "s1")
        // Cursor must be retained so reconnect sends from_seq=10
        XCTAssertEqual(machine.fromSeq(for: "s1"), 10)
    }

    // MARK: - Multi-session isolation

    func testMultipleSessionsAreisolated() {
        let machine = ResyncMachine()
        machine.applySnapshot(sessionId: "s1", events: [makeEvent(sessionId: "s1", seq: 1)], cursorSeq: 1)
        machine.applySnapshot(sessionId: "s2", events: [makeEvent(sessionId: "s2", seq: 10)], cursorSeq: 10)

        XCTAssertEqual(machine.cursor(for: "s1"), 1)
        XCTAssertEqual(machine.cursor(for: "s2"), 10)
        XCTAssertEqual(machine.events(for: "s1").count, 1)
        XCTAssertEqual(machine.events(for: "s2").count, 1)
    }
}
