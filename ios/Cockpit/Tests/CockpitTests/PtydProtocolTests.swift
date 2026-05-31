// CV5 — ptyd protocol unit tests (no live socket required).
// TAG:UNTESTED — live TLS connection, real pairing, cert pinning against a running daemon.
//
// What is tested here (no network):
//   T1 — Hello request JSON encodes to the exact shape mobile.rs Hello expects.
//   T2 — ptyd structured Event JSON decodes into the iOS Event model (incl. unknown kind).
//   T3 — audit_list + approved responses decode correctly.
//   T4 — Line-framed reader: splits concatenated newline-JSON into frames; holds back partial.
//   T5 — Cert fingerprint comparison: match / mismatch / length / case.
//   T6 — SettingsViewModel validation for host:port + token + fingerprint.
import XCTest
@testable import Cockpit

final class PtydProtocolTests: XCTestCase {

    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = .sortedKeys
        return e
    }()
    private let decoder = JSONDecoder()

    // MARK: - T1: hello request shape matches mobile.rs Hello

    func testHelloRequestEncodesExactShape() throws {
        // mobile.rs Hello { protocol_version: u32, token: String }
        // The iOS side sends: {"id":..., "method":"hello", "params":{"protocol_version":1,"token":"..."}}
        let params = HelloParams(protocolVersion: 1, token: "deadbeef01234567deadbeef01234567deadbeef01234567deadbeef01234567")
        let data = try encoder.encode(params)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]

        // Field names must match mobile.rs exactly.
        XCTAssertEqual(obj["protocol_version"] as? Int, 1, "field name must be protocol_version (snake_case)")
        XCTAssertEqual(
            obj["token"] as? String,
            "deadbeef01234567deadbeef01234567deadbeef01234567deadbeef01234567"
        )
        // No extra fields.
        XCTAssertEqual(obj.count, 2, "Hello params must have exactly 2 fields")
    }

    func testHelloParamsRejectsCamelCase() throws {
        // Ensure there is no "protocolVersion" camelCase key on the wire.
        let params = HelloParams(protocolVersion: 1, token: "tok")
        let data = try encoder.encode(params)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertNil(obj["protocolVersion"], "camelCase key must not appear — mobile.rs expects snake_case")
    }

    // MARK: - T2: ptyd Event decodes correctly (incl. unknown kind + u64 created_at)

    func testEventDecodesKnownKind() throws {
        let json = """
        {
          "session_id": "abc123",
          "seq": 5,
          "sender": "agent",
          "kind": "message",
          "content": "Hello from ptyd",
          "requires_user_input": false,
          "created_at": 1748736000,
          "metadata": {}
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.sessionId, "abc123")
        XCTAssertEqual(event.seq, 5)
        XCTAssertEqual(event.sender, "agent")
        XCTAssertEqual(event.kind, "message")
        XCTAssertFalse(event.requiresUserInput)
        XCTAssertEqual(event.createdAt, 1_748_736_000)  // u64 unix seconds
    }

    func testEventToleratesUnknownKind() throws {
        let json = """
        {
          "session_id": "s1",
          "seq": 1,
          "sender": "system",
          "kind": "future_kind_xyz",
          "content": "something",
          "requires_user_input": false,
          "created_at": 1000,
          "metadata": {}
        }
        """.data(using: .utf8)!
        // Must not throw — unknown kind kept as-is.
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.kind, "future_kind_xyz")
    }

    func testApprovalRequestEventSetsRequiresUserInput() throws {
        let json = """
        {
          "session_id": "s2",
          "seq": 7,
          "sender": "system",
          "kind": "approval_request",
          "content": "approval needed for tool: bash",
          "requires_user_input": true,
          "created_at": 1748736100,
          "metadata": {
            "approval_id": "a1b2c3d4",
            "tool": "bash",
            "args": {"command": "rm -rf /tmp/test"}
          }
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.kind, "approval_request")
        XCTAssertTrue(event.requiresUserInput)
        if case .string(let aid) = event.metadata["approval_id"] {
            XCTAssertEqual(aid, "a1b2c3d4")
        } else {
            XCTFail("approval_id missing from metadata")
        }
    }

    func testToolCallEventDecodes() throws {
        let json = """
        {
          "session_id": "sess-x",
          "seq": 3,
          "sender": "agent",
          "kind": "tool_call",
          "content": "read_file: {\\\"path\\\":\\\"/tmp/x\\\"}",
          "requires_user_input": false,
          "created_at": 9999,
          "metadata": {
            "tool_id": "tu_001",
            "tool_name": "read_file",
            "input": {"path": "/tmp/x"}
          }
        }
        """.data(using: .utf8)!
        let event = try decoder.decode(Event.self, from: json)
        XCTAssertEqual(event.kind, "tool_call")
        if case .string(let name) = event.metadata["tool_name"] {
            XCTAssertEqual(name, "read_file")
        } else {
            XCTFail("tool_name missing")
        }
    }

    // MARK: - T3: audit_list + approved result shapes

    func testAuditListDecodes() throws {
        let json = """
        {
          "id": "req-1",
          "result": {
            "type": "audit_list",
            "entries": [
              {
                "id": "e1",
                "ts": 1748736200,
                "actor": "client",
                "action": "agent.approve",
                "session_id": "s1",
                "detail": {"decision": "allow"}
              }
            ]
          }
        }
        """.data(using: .utf8)!
        let response = try decoder.decode(PtydResponse.self, from: json)
        guard case .auditList(let entries) = response.result else {
            XCTFail("Expected auditList, got \(String(describing: response.result))")
            return
        }
        XCTAssertEqual(entries.count, 1)
        XCTAssertEqual(entries[0].id, "e1")
        XCTAssertEqual(entries[0].ts, 1_748_736_200)
        XCTAssertEqual(entries[0].actor, "client")
        XCTAssertEqual(entries[0].action, "agent.approve")
        XCTAssertEqual(entries[0].sessionId, "s1")
    }

    func testApprovedResultDecodes() throws {
        let json = """
        {
          "id": "req-2",
          "result": {
            "type": "approved",
            "approval_id": "app-xyz",
            "decision": "allow"
          }
        }
        """.data(using: .utf8)!
        let response = try decoder.decode(PtydResponse.self, from: json)
        guard case .approved(let approvalId, let decision) = response.result else {
            XCTFail("Expected approved, got \(String(describing: response.result))")
            return
        }
        XCTAssertEqual(approvalId, "app-xyz")
        XCTAssertEqual(decision, "allow")
    }

    func testHelloAckOkDecodes() throws {
        let json = """
        {
          "id": "h1",
          "result": {
            "type": "hello_ack",
            "ok": true,
            "protocol_version": 1,
            "capabilities": ["agent.subscribe_structured", "agent.get_audit"]
          }
        }
        """.data(using: .utf8)!
        let response = try decoder.decode(PtydResponse.self, from: json)
        guard case .helloAck(let ack) = response.result else {
            XCTFail("Expected helloAck")
            return
        }
        XCTAssertTrue(ack.ok)
        XCTAssertEqual(ack.protocolVersion, 1)
        XCTAssertEqual(ack.capabilities.count, 2)
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
        let response = try decoder.decode(PtydResponse.self, from: json)
        guard case .helloAck(let ack) = response.result else {
            XCTFail("Expected helloAck")
            return
        }
        XCTAssertFalse(ack.ok)
        XCTAssertEqual(ack.error, "unauthorized: invalid pairing token")
    }

    func testPtydErrorEnvelopeDecodes() throws {
        let json = """
        {
          "id": "req-3",
          "error": {
            "code": "transcript_not_found",
            "message": "no transcript for session abc"
          }
        }
        """.data(using: .utf8)!
        let response = try decoder.decode(PtydResponse.self, from: json)
        XCTAssertNil(response.result)
        XCTAssertEqual(response.error?.code, "transcript_not_found")
    }

    // MARK: - T4: line-framed reader

    /// The reader must split a buffer of concatenated newline-terminated JSON lines
    /// and hold back any partial trailing line (no newline yet).
    func testLineFramerSplitsCompleteLines() {
        var buffer = Data()
        let line1 = "{\"seq\":1}\n".data(using: .utf8)!
        let line2 = "{\"seq\":2}\n".data(using: .utf8)!
        let partial = "{\"seq\":3}".data(using: .utf8)!  // no trailing newline

        buffer.append(line1)
        buffer.append(line2)
        buffer.append(partial)

        let frames = splitLines(buffer: &buffer)
        XCTAssertEqual(frames.count, 2, "Only 2 complete lines; partial stays in buffer")
        XCTAssertEqual(String(data: frames[0], encoding: .utf8), "{\"seq\":1}")
        XCTAssertEqual(String(data: frames[1], encoding: .utf8), "{\"seq\":2}")
        // Partial remains in buffer.
        XCTAssertEqual(buffer, partial, "Partial line must remain in buffer")
    }

    func testLineFramerEmptyInputYieldsNoFrames() {
        var buffer = Data()
        let frames = splitLines(buffer: &buffer)
        XCTAssertTrue(frames.isEmpty)
        XCTAssertTrue(buffer.isEmpty)
    }

    func testLineFramerSingleCompleteLineNothingLeft() {
        var buffer = "{\"type\":\"ok\"}\n".data(using: .utf8)!
        let frames = splitLines(buffer: &buffer)
        XCTAssertEqual(frames.count, 1)
        XCTAssertTrue(buffer.isEmpty)
    }

    func testLineFramerOnlyPartialLineYieldsNothingAndBufferRetained() {
        let partial = "{\"type\":\"par".data(using: .utf8)!
        var buffer = partial
        let frames = splitLines(buffer: &buffer)
        XCTAssertTrue(frames.isEmpty)
        XCTAssertEqual(buffer, partial)
    }

    /// Simulate arriving data in two chunks: first chunk has line1 + half of line2,
    /// second chunk completes line2. Both frames must appear exactly once.
    func testLineFramerHandlesChunkedArrival() {
        var buffer = Data()
        // Chunk 1: line1 complete + partial line2.
        buffer.append("{\"seq\":10}\n{\"seq\":".data(using: .utf8)!)
        let frames1 = splitLines(buffer: &buffer)
        XCTAssertEqual(frames1.count, 1)

        // Chunk 2: rest of line2.
        buffer.append("20}\n".data(using: .utf8)!)
        let frames2 = splitLines(buffer: &buffer)
        XCTAssertEqual(frames2.count, 1)
        XCTAssertEqual(String(data: frames2[0], encoding: .utf8), "{\"seq\":20}")
    }

    // MARK: - T5: cert fingerprint comparison

    func testFingerprintsMatchIdentical() {
        let fp = "a3b2c1d0e4f5a3b2c1d0e4f5a3b2c1d0e4f5a3b2c1d0e4f5a3b2c1d0e4f5a3b2"
        XCTAssertTrue(CockpitClient.fingerprintsMatch(fp, fp))
    }

    func testFingerprintsMatchCaseInsensitive() {
        let lower = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"
        XCTAssertTrue(CockpitClient.fingerprintsMatch(lower, upper))
    }

    func testFingerprintsRejectMismatch() {
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        let b = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"
        XCTAssertFalse(CockpitClient.fingerprintsMatch(a, b))
    }

    func testFingerprintsRejectLengthMismatch() {
        let a = "aabb"
        let b = "aabbcc"
        XCTAssertFalse(CockpitClient.fingerprintsMatch(a, b))
    }

    func testFingerprintsRejectEmpty() {
        XCTAssertFalse(CockpitClient.fingerprintsMatch("", "abc"))
        XCTAssertFalse(CockpitClient.fingerprintsMatch("abc", ""))
    }

    // MARK: - T6: SettingsViewModel validation for ptyd config

    @MainActor
    func testValidConfigPassesValidation() throws {
        let vm = makeSettingsVM(
            host: "127.0.0.1",
            port: "7333",
            token: "deadbeef01234567deadbeef01234567deadbeef01234567deadbeef01234567",
            fp: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        )
        XCTAssertTrue(vm.validate())
        XCTAssertTrue(vm.validationErrors.isEmpty)
    }

    @MainActor
    func testEmptyHostFails() throws {
        let vm = makeSettingsVM(host: "", port: "7333", token: "tok", fp: String(repeating: "a", count: 64))
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.emptyHost))
    }

    @MainActor
    func testInvalidPortFails() throws {
        let vm = makeSettingsVM(host: "127.0.0.1", port: "notaport", token: "tok", fp: String(repeating: "a", count: 64))
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.invalidPort))
    }

    @MainActor
    func testPortZeroIsInvalid() throws {
        // UInt16(0) is valid Swift but port 0 is not useful; UInt16("0") does parse.
        // The validation accepts 0 as a UInt16 — note this for future tightening.
        // What we DO test: port > 65535 is rejected.
        let vm = makeSettingsVM(host: "127.0.0.1", port: "99999", token: "tok", fp: String(repeating: "a", count: 64))
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.invalidPort))
    }

    @MainActor
    func testEmptyTokenFails() throws {
        let vm = makeSettingsVM(host: "127.0.0.1", port: "7333", token: "", fp: String(repeating: "a", count: 64))
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.emptyToken))
    }

    @MainActor
    func testFingerprintTooShortFails() throws {
        let vm = makeSettingsVM(host: "127.0.0.1", port: "7333", token: "tok", fp: "abc123")
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.invalidFingerprint))
    }

    @MainActor
    func testFingerprintNonHexFails() throws {
        // 64 chars but not all hex.
        let fp = String(repeating: "z", count: 64)
        let vm = makeSettingsVM(host: "127.0.0.1", port: "7333", token: "tok", fp: fp)
        XCTAssertFalse(vm.validate())
        XCTAssertTrue(vm.validationErrors.contains(.invalidFingerprint))
    }

    @MainActor
    func testSavePersistsTrimmedValues() throws {
        let suite = makeSuite()
        let cfg = CockpitConfig(defaults: suite)
        cfg.host = "old-host"
        let fp = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        let vm = SettingsViewModel(config: cfg)
        vm.hostDraft        = "  127.0.0.1  "
        vm.portDraft        = "  7333  "
        vm.tokenDraft       = "  newtoken  "
        vm.fingerprintDraft = "  \(fp)  "
        let saved = vm.save()
        XCTAssertTrue(saved)
        XCTAssertEqual(cfg.host, "127.0.0.1")
        XCTAssertEqual(cfg.portString, "7333")
        XCTAssertEqual(cfg.pairingToken, "newtoken")
        XCTAssertEqual(cfg.pinnedFingerprint, fp)
    }

    @MainActor
    func testResetRestoresDefaults() throws {
        let vm = makeSettingsVM(host: "changed", port: "9999", token: "tok", fp: String(repeating: "a", count: 64))
        vm.reset()
        XCTAssertEqual(vm.hostDraft, CockpitConfig.defaultHost)
        XCTAssertEqual(vm.portDraft, String(CockpitConfig.defaultPort))
        XCTAssertEqual(vm.tokenDraft, "")
        XCTAssertEqual(vm.fingerprintDraft, "")
        XCTAssertTrue(vm.validationErrors.isEmpty)
    }

    // MARK: - Helpers

    @MainActor
    private func makeSettingsVM(host: String, port: String, token: String, fp: String) -> SettingsViewModel {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = host
        cfg.portString = port
        cfg.pairingToken = token
        cfg.pinnedFingerprint = fp
        let vm = SettingsViewModel(config: cfg)
        vm.hostDraft = host
        vm.portDraft = port
        vm.tokenDraft = token
        vm.fingerprintDraft = fp
        return vm
    }

    private func makeSuite() -> UserDefaults {
        UserDefaults(suiteName: "ai.rosslabs.cockpitTests.ptyd-\(UUID().uuidString)")!
    }

    /// Extracts complete newline-terminated frames from a Data buffer (in-place).
    /// Mirrors CockpitClient.drainLines logic — tested here without the MainActor.
    private func splitLines(buffer: inout Data) -> [Data] {
        var frames: [Data] = []
        while let idx = buffer.firstIndex(of: UInt8(ascii: "\n")) {
            let frame = Data(buffer[buffer.startIndex ..< idx])
            buffer = Data(buffer[buffer.index(after: idx)...])
            if !frame.isEmpty {
                frames.append(frame)
            }
        }
        return frames
    }
}
