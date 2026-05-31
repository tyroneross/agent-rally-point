// D1 tests — wire model decode round-trips + tolerance invariants
import XCTest
@testable import Cockpit

final class WireModelTests: XCTestCase {

    private let decoder = JSONDecoder()

    // MARK: - ServerFrame round-trips

    func testHelloOkDecodes() throws {
        let json = """
        {"t":"hello_ok","server_version":"0.1.0","protocol":1}
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .helloOk(let p) = frame else { XCTFail("Expected helloOk"); return }
        XCTAssertEqual(p.serverVersion, "0.1.0")
        XCTAssertEqual(p.protocol, 1)
    }

    func testPongDecodes() throws {
        let json = "{\"t\":\"pong\"}".data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .pong = frame else { XCTFail("Expected pong"); return }
    }

    func testErrorDecodes() throws {
        let json = """
        {"t":"error","code":"unauthorized","message":"bad token"}
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .error(let p) = frame else { XCTFail("Expected error"); return }
        XCTAssertEqual(p.code, "unauthorized")
    }

    func testSessionListDecodes() throws {
        let json = """
        {
          "t":"session_list",
          "sessions":[{
            "id":"abc123","owner_id":"u1","agent_type":"claude",
            "repo_path":"/repo","status":"active","title":null,
            "created_at":"2026-05-31T00:00:00Z","last_seq":5
          }]
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .sessionList(let p) = frame else { XCTFail("Expected sessionList"); return }
        XCTAssertEqual(p.sessions.count, 1)
        XCTAssertEqual(p.sessions[0].agentType, "claude")
        XCTAssertEqual(p.sessions[0].status, .active)
    }

    func testSnapshotDecodes() throws {
        let json = """
        {
          "t":"snapshot",
          "session_id":"s1",
          "session":{"id":"s1","owner_id":"u1","agent_type":"codex","repo_path":"/r",
                     "status":"awaiting_input","title":"Test","created_at":"2026-05-31T00:00:00Z","last_seq":2},
          "events":[
            {"session_id":"s1","seq":1,"sender":"user","kind":"message",
             "content":"hello","requires_user_input":false,
             "created_at":"2026-05-31T00:00:00Z","metadata":{}}
          ],
          "cursor_seq":1
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .snapshot(let p) = frame else { XCTFail("Expected snapshot"); return }
        XCTAssertEqual(p.sessionId, "s1")
        XCTAssertEqual(p.events.count, 1)
        XCTAssertEqual(p.cursorSeq, 1)
    }

    func testEventFrameDecodes() throws {
        let json = """
        {
          "t":"event",
          "session_id":"s1",
          "event":{
            "session_id":"s1","seq":3,"sender":"agent","kind":"tool_call",
            "content":"ls -la","requires_user_input":false,
            "created_at":"2026-05-31T00:00:00Z","metadata":{"cmd":"ls"}
          }
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .event(let p) = frame else { XCTFail("Expected event"); return }
        XCTAssertEqual(p.event.kind, "tool_call")
        XCTAssertEqual(p.event.seq, 3)
    }

    func testApprovalRequestDecodes() throws {
        let json = """
        {
          "t":"approval_request",
          "approval":{
            "id":"app1","session_id":"s1","event_seq":4,"tool":"bash",
            "args":{"cmd":"rm -rf /tmp/test"},"created_at":"2026-05-31T00:00:00Z",
            "ttl_secs":30,"resolution":null
          }
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .approvalRequest(let p) = frame else { XCTFail("Expected approvalRequest"); return }
        XCTAssertEqual(p.approval.tool, "bash")
        XCTAssertNil(p.approval.resolution)
    }

    // MARK: - Tolerance invariants (wire contract §Invariants 1 & 2)

    /// Unknown `t` must NOT throw — decodes to .unknown
    func testUnknownFrameTypeToleratedNotThrown() throws {
        let json = """
        {"t":"future_frame_type","some_field":"value"}
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .unknown(let t) = frame else { XCTFail("Expected .unknown, got \(frame)"); return }
        XCTAssertEqual(t, "future_frame_type")
    }

    /// Unknown `kind` must NOT throw — kind is kept as-is String
    func testUnknownEventKindTolerated() throws {
        let json = """
        {
          "t":"event",
          "session_id":"s1",
          "event":{
            "session_id":"s1","seq":7,"sender":"agent","kind":"future_kind",
            "content":"something","requires_user_input":false,
            "created_at":"2026-05-31T00:00:00Z","metadata":{}
          }
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .event(let p) = frame else { XCTFail("Expected .event"); return }
        XCTAssertEqual(p.event.kind, "future_kind")   // open string, no throw
    }

    /// Unknown `agent_type` must NOT throw — kept as-is String
    func testUnknownAgentTypeTolerated() throws {
        let json = """
        {
          "t":"session_list",
          "sessions":[{
            "id":"xyz","owner_id":"u1","agent_type":"gemini",
            "repo_path":"/r","status":"active","title":null,
            "created_at":"2026-05-31T00:00:00Z","last_seq":0
          }]
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .sessionList(let p) = frame else { XCTFail(); return }
        XCTAssertEqual(p.sessions[0].agentType, "gemini")   // open string
    }

    /// Unknown `status` must NOT throw — decodes to .unknown
    func testUnknownStatusTolerated() throws {
        let json = """
        {
          "t":"session_status",
          "session_id":"s1",
          "status":"initializing"
        }
        """.data(using: .utf8)!
        let frame = try decoder.decode(ServerFrame.self, from: json)
        guard case .sessionStatus(let p) = frame else { XCTFail(); return }
        guard case .unknown(let raw) = p.status else { XCTFail("Expected .unknown status, got \(p.status)"); return }
        XCTAssertEqual(raw, "initializing")
    }

    /// Compound tolerance: event with future_kind + gemini agent_type + unknown frame t
    func testCompoundForwardCompatTolerance() throws {
        // 1. Unknown frame type
        let unknownFrame = """
        {"t":"x_new_frame","payload":"data"}
        """.data(using: .utf8)!
        let f1 = try decoder.decode(ServerFrame.self, from: unknownFrame)
        guard case .unknown = f1 else { XCTFail("Should be .unknown"); return }

        // 2. Event with kind "future_kind" and sender includes new agent "gemini"
        let eventJSON = """
        {
          "t":"event",
          "session_id":"s2",
          "event":{
            "session_id":"s2","seq":99,"sender":"gemini",
            "kind":"future_kind","content":"hi",
            "requires_user_input":false,"created_at":"2026-05-31T00:00:00Z","metadata":{}
          }
        }
        """.data(using: .utf8)!
        let f2 = try decoder.decode(ServerFrame.self, from: eventJSON)
        guard case .event(let p) = f2 else { XCTFail("Should be .event"); return }
        XCTAssertEqual(p.event.kind, "future_kind")
        XCTAssertEqual(p.event.sender, "gemini")
    }

    // MARK: - ClientCommand encode

    func testHelloCommandEncodes() throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = .sortedKeys
        let cmd = ClientCommand.hello(token: "tok123")
        let data = try encoder.encode(cmd)
        let dict = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        XCTAssertEqual(dict?["t"] as? String, "hello")
        XCTAssertEqual(dict?["token"] as? String, "tok123")
        XCTAssertEqual(dict?["protocol"] as? Int, 1)
    }

    func testOpenSessionCommandEncodes() throws {
        let encoder = JSONEncoder()
        let cmd = ClientCommand.openSession(sessionId: "s1", fromSeq: 42)
        let data = try encoder.encode(cmd)
        let dict = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        XCTAssertEqual(dict?["t"] as? String, "open_session")
        XCTAssertEqual(dict?["session_id"] as? String, "s1")
        // JSON numbers may come back as Int or Double
        let fromSeq = dict?["from_seq"]
        XCTAssertTrue(fromSeq is Int || fromSeq is Double)
    }

    func testApproveCommandEncodes() throws {
        let encoder = JSONEncoder()
        let cmd = ClientCommand.approve(approvalId: "app1", decision: .allow, reason: nil)
        let data = try encoder.encode(cmd)
        let dict = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        XCTAssertEqual(dict?["t"] as? String, "approve")
        XCTAssertEqual(dict?["decision"] as? String, "allow")
        XCTAssertNil(dict?["reason"])
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
        let dec = JSONDecoder()
        for (raw, expected) in cases {
            let data = "\"\(raw)\"".data(using: .utf8)!
            let status = try dec.decode(SessionStatus.self, from: data)
            XCTAssertEqual(status, expected, "Failed for \(raw)")
        }
    }
}
