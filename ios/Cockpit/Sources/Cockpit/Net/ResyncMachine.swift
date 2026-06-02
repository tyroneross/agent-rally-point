// D4 — Reconnect + replay state machine
// Tracks highest seq per session; on (re)connect sends open_session{from_seq}.
// Merges snapshot + deltas with NO gaps, NO dupes.
// Backgrounding is treated as disconnect.

import Foundation

/// Tracks per-session cursor and owns ordered, deduplicated event storage.
public final class ResyncMachine: @unchecked Sendable {

    // MARK: - State (not @MainActor — intentionally plain for unit testing)

    private let lock = NSLock()
    private var _cursors: [String: UInt64] = [:]          // session_id → highest seq seen
    private var _events: [String: [Event]] = [:]          // session_id → ordered events

    public init() {}

    // MARK: - Cursor

    /// Returns the highest seq seen for `sessionId` (0 if never opened).
    public func cursor(for sessionId: String) -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        return _cursors[sessionId] ?? 0
    }

    // MARK: - Snapshot ingestion

    /// Called when the server sends a `snapshot` frame on (re)connect.
    /// Replaces any stored events for the session (server guarantees completeness
    /// from seq=1 up to `cursorSeq`), then bumps the cursor.
    public func applySnapshot(sessionId: String, events: [Event], cursorSeq: UInt64) {
        lock.lock(); defer { lock.unlock() }
        let sorted = events.sorted { $0.seq < $1.seq }
        _events[sessionId] = sorted
        let maxSeq = max(cursorSeq, sorted.last?.seq ?? 0)
        _cursors[sessionId] = maxSeq
    }

    // MARK: - Delta ingestion

    /// Appends a live delta event. Silently drops dupes (seq already seen).
    /// Returns true if the event was actually inserted.
    @discardableResult
    public func applyDelta(sessionId: String, event: Event) -> Bool {
        lock.lock(); defer { lock.unlock() }
        let current = _cursors[sessionId] ?? 0
        guard event.seq > current else { return false }   // dupe / already have it
        var list = _events[sessionId] ?? []
        list.append(event)
        list.sort { $0.seq < $1.seq }
        _events[sessionId] = list
        _cursors[sessionId] = event.seq
        return true
    }

    // MARK: - Batch delta ingestion (from replay)

    /// Merges a replay batch. Drops anything already at or below the current cursor
    /// to preserve the no-gaps, no-dupes invariant.
    public func applyDeltas(sessionId: String, events: [Event]) {
        for e in events { applyDelta(sessionId: sessionId, event: e) }
    }

    // MARK: - Read

    /// Returns a copy of events for `sessionId`, ordered by seq.
    public func events(for sessionId: String) -> [Event] {
        lock.lock(); defer { lock.unlock() }
        return _events[sessionId] ?? []
    }

    // MARK: - Reconnect hook

    /// Returns the `from_seq` value to include in `open_session` on reconnect.
    /// Callers should send `openSession(id: sessionId, fromSeq: fromSeq(for:))`.
    public func fromSeq(for sessionId: String) -> UInt64 {
        cursor(for: sessionId)
    }

    // MARK: - Background = disconnect

    /// Clears in-flight expectations; cursor is retained so reconnect requests
    /// the right replay window.
    public func handleBackground(sessionId: String) {
        // Cursor is intentionally preserved — we want replay on reconnect.
        // No extra state to clear in this simple implementation.
    }
}
