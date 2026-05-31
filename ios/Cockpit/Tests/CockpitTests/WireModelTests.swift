// CV5 — Wire model tests (ptyd protocol shapes).
// Replaces cockpit WebSocket ServerFrame / ClientCommand tests with ptyd equivalents.
import XCTest
@testable import Cockpit

final class WireModelTests: XCTestCase {

    private let decoder = JSONDecoder()

    // MARK: - PtydResponse / PtydResult

    func testHelloAckOkDecodes() throws {
        let json = """
        {
          "id": "h1",
          "result": {
            "type": "hello_ack",
            "ok": true,
            "protocol_version": 1,
            "capabilities": ["agent.subscribe_structured", "agent.get_audit", "agent.approve"]
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .helloAck(let ack) = r.result else {
            XCTFail("Expected .helloAck"); return
        }
        XCTAssertTrue(ack.ok)
        XCTAssertEqual(ack.protocolVersion, 1)
        XCTAssertEqual(ack.capabilities.count, 3)
        XCTAssertNil(ack.error)
    }

    func testHelloAckRejectedDecodes() throws {
        let json = """
        {
          "id": "h1",
          "result": {
            "type": "hello_ack",
            "ok": false,
            "protocol_version": 1,
            "error": "unauthorized: invalid pairing token",
            "capabilities": []
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .helloAck(let ack) = r.result else {
            XCTFail("Expected .helloAck"); return
        }
        XCTAssertFalse(ack.ok)
        XCTAssertEqual(ack.error, "unauthorized: invalid pairing token")
    }

    func testStructuredSubscriptionStartedDecodes() throws {
        let json = """
        {
          "id": "req-1",
          "result": {
            "type": "structured_subscription_started",
            "session_id": "abc123"
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .structuredSubscriptionStarted(let sid) = r.result else {
            XCTFail("Expected .structuredSubscriptionStarted"); return
        }
        XCTAssertEqual(sid, "abc123")
    }

    func testAuditListDecodes() throws {
        let json = """
        {
          "id": "req-2",
          "result": {
            "type": "audit_list",
            "entries": [
              {
                "id": "e1",
                "ts": 1748736000,
                "actor": "client",
                "action": "agent.approve",
                "session_id": "s1",
                "detail": {"decision": "allow"}
              }
            ]
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .auditList(let entries) = r.result else {
            XCTFail("Expected .auditList"); return
        }
        XCTAssertEqual(entries.count, 1)
        XCTAssertEqual(entries[0].ts, 1_748_736_000)
        XCTAssertEqual(entries[0].actor, "client")
    }

    func testApprovedDecodes() throws {
        let json = """
        {
          "id": "req-3",
          "result": {
            "type": "approved",
            "approval_id": "app-xyz",
            "decision": "deny"
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .approved(let aid, let decision) = r.result else {
            XCTFail("Expected .approved"); return
        }
        XCTAssertEqual(aid, "app-xyz")
        XCTAssertEqual(decision, "deny")
    }

    func testPtydErrorDecodes() throws {
        let json = """
        {
          "id": "req-4",
          "error": {
            "code": "transcript_not_found",
            "message": "no transcript for session abc"
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        XCTAssertNil(r.result)
        XCTAssertEqual(r.error?.code, "transcript_not_found")
    }

    func testUnknownResultTypeDecodes() throws {
        let json = """
        {
          "id": "req-5",
          "result": {
            "type": "future_result_type_xyz"
          }
        }
        """.data(using: .utf8)!
        let r = try decoder.decode(PtydResponse.self, from: json)
        guard case .unknown(let type_) = r.result else {
            XCTFail("Expected .unknown result"); return
        }
        XCTAssertEqual(type_, "future_result_type_xyz")
    }

    // MARK: - Event model

    func testEventDecodesAllFields() throws {
        let json = """
        {
          "session_id": "sess-001",
          "seq": 12,
          "sender": "agent",
          "kind": "tool_call",
          "content": "bash: ls -la",
          "requires_user_input": false,
          "created_at": 1748736100,
          "metadata": {"tool_name": "bash"}
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.sessionId, "sess-001")
        XCTAssertEqual(event.seq, 12)
        XCTAssertEqual(event.sender, "agent")
        XCTAssertEqual(event.kind, "tool_call")
        XCTAssertEqual(event.createdAt, 1_748_736_100)
        XCTAssertFalse(event.requiresUserInput)
    }

    /// created_at must be UInt64 (Unix seconds), not a date string — matches ptyd structured.rs.
    func testCreatedAtIsUInt64NotString() throws {
        let json = """
        {
          "session_id": "s1", "seq": 1, "sender": "agent",
          "kind": "message", "content": "hi",
          "requires_user_input": false,
          "created_at": 9999999999,
          "metadata": {}
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.createdAt, 9_999_999_999)
    }

    /// Unknown kind must not throw — open string.
    func testUnknownEventKindTolerated() throws {
        let json = """
        {
          "session_id": "s2", "seq": 5, "sender": "system",
          "kind": "future_kind_xyz", "content": "?",
          "requires_user_input": false, "created_at": 1, "metadata": {}
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.kind, "future_kind_xyz")
    }

    // MARK: - SessionStatus round-trips

    func testAllKnownStatusesRoundTrip() throws {
        let cases: [(String, SessionStatus)] = [
            ("active", .active),
            ("awaiting_input", .awaitingInput),
            ("paused", .paused),
            ("stale", .stale),
            ("completed", .completed),
            ("failed", .failed),
            ("killed", .killed),
            ("disconnected", .disconnected),
        ]
        for (raw, expected) in cases {
            let data = "\"\(raw)\"".data(using: .utf8)!
            let status = try decoder.decode(SessionStatus.self, from: data)
            XCTAssertEqual(status, expected, "Failed for \(raw)")
        }
    }

    func testUnknownStatusTolerated() throws {
        let data = "\"initializing\"".data(using: .utf8)!
        let status = try decoder.decode(SessionStatus.self, from: data)
        guard case .unknown(let raw) = status else {
            XCTFail("Expected .unknown, got \(status)"); return
        }
        XCTAssertEqual(raw, "initializing")
    }

    // MARK: - JSONValue

    func testJSONValueBoolParsedAsBoolNotDouble() throws {
        // Regression: Bool must be decoded before Double.
        let trueData = "true".data(using: .utf8)!
        let v = try decoder.decode(JSONValue.self, from: trueData)
        guard case .bool(let b) = v else {
            XCTFail("Expected .bool, got \(v)"); return
        }
        XCTAssertTrue(b)
    }
}
