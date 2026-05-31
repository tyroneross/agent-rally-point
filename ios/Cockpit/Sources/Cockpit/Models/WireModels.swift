// D1 — Wire models mirroring COCKPIT-WIRE.md (contract v1)
// Invariant: unknown `t`, `kind`, `status`, `agent_type` MUST NOT throw on decode.

import Foundation

// MARK: - Domain types

/// Session status. Open string — unknown values map to `.unknown`.
public enum SessionStatus: Codable, Equatable, Sendable {
    case active
    case awaitingInput
    case paused
    case stale
    case completed
    case failed
    case killed
    case disconnected
    case unknown(String)

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        switch raw {
        case "active":           self = .active
        case "awaiting_input":   self = .awaitingInput
        case "paused":           self = .paused
        case "stale":            self = .stale
        case "completed":        self = .completed
        case "failed":           self = .failed
        case "killed":           self = .killed
        case "disconnected":     self = .disconnected
        default:                 self = .unknown(raw)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .active:           try c.encode("active")
        case .awaitingInput:    try c.encode("awaiting_input")
        case .paused:           try c.encode("paused")
        case .stale:            try c.encode("stale")
        case .completed:        try c.encode("completed")
        case .failed:           try c.encode("failed")
        case .killed:           try c.encode("killed")
        case .disconnected:     try c.encode("disconnected")
        case .unknown(let raw): try c.encode(raw)
        }
    }
}

/// Free-form JSON object (metadata / args).
public typealias JSONObject = [String: JSONValue]

/// A type-erased JSON value so metadata/args remain free-form.
public enum JSONValue: Codable, Equatable, Sendable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case null
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let v = try? c.decode(String.self)  { self = .string(v); return }
        if let v = try? c.decode(Double.self)  { self = .number(v); return }
        if let v = try? c.decode(Bool.self)    { self = .bool(v);   return }
        if c.decodeNil()                        { self = .null;       return }
        if let v = try? c.decode([JSONValue].self)          { self = .array(v);  return }
        if let v = try? c.decode([String: JSONValue].self)  { self = .object(v); return }
        self = .null
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let v):  try c.encode(v)
        case .number(let v):  try c.encode(v)
        case .bool(let v):    try c.encode(v)
        case .null:           try c.encodeNil()
        case .array(let v):   try c.encode(v)
        case .object(let v):  try c.encode(v)
        }
    }
}

/// Session domain object.
public struct Session: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let ownerId: String
    /// Open string — `claude`, `codex`, or a future agent type.
    public let agentType: String
    public let repoPath: String
    public let status: SessionStatus
    public let title: String?
    public let createdAt: String
    public let lastSeq: UInt64

    private enum CodingKeys: String, CodingKey {
        case id, title, status
        case ownerId    = "owner_id"
        case agentType  = "agent_type"
        case repoPath   = "repo_path"
        case createdAt  = "created_at"
        case lastSeq    = "last_seq"
    }
}

/// Event domain object.
/// `kind` is an open string: message | tool_call | tool_result | diff | status |
/// approval_request | error — and future values must not throw.
public struct Event: Codable, Identifiable, Equatable, Sendable {
    public let sessionId: String
    public let seq: UInt64
    public let sender: String        // "agent" | "user" | "system"
    /// Open string — unknown kinds render as generic.
    public let kind: String
    public let content: String
    public let requiresUserInput: Bool
    public let createdAt: String
    public let metadata: JSONObject

    public var id: String { "\(sessionId)-\(seq)" }

    private enum CodingKeys: String, CodingKey {
        case seq, sender, kind, content, metadata
        case sessionId         = "session_id"
        case requiresUserInput = "requires_user_input"
        case createdAt         = "created_at"
    }
}

/// Approval domain object.
public struct Approval: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let sessionId: String
    public let eventSeq: UInt64
    public let tool: String
    public let args: JSONObject
    public let createdAt: String
    public let ttlSecs: UInt64
    /// null | "allow" | "deny" | "auto_denied" | "aborted"
    public let resolution: String?

    private enum CodingKeys: String, CodingKey {
        case id, tool, args, resolution
        case sessionId  = "session_id"
        case eventSeq   = "event_seq"
        case createdAt  = "created_at"
        case ttlSecs    = "ttl_secs"
    }
}

// MARK: - Server → client frames

/// Incoming frame discriminated by the `t` field.
/// Unknown `t` values decode to `.unknown(raw, payload)` — NEVER throw.
public enum ServerFrame: Sendable {
    case helloOk(HelloOkPayload)
    case error(ErrorPayload)
    case sessionList(SessionListPayload)
    case snapshot(SnapshotPayload)
    case event(EventPayload)
    case sessionStatus(SessionStatusPayload)
    case approvalRequest(ApprovalRequestPayload)
    case pong
    case unknown(t: String)
}

extension ServerFrame: Decodable {
    private enum TopKey: String, CodingKey { case t }

    public init(from decoder: Decoder) throws {
        let top = try decoder.container(keyedBy: TopKey.self)
        let t = try top.decode(String.self, forKey: .t)
        switch t {
        case "hello_ok":
            self = .helloOk(try HelloOkPayload(from: decoder))
        case "error":
            self = .error(try ErrorPayload(from: decoder))
        case "session_list":
            self = .sessionList(try SessionListPayload(from: decoder))
        case "snapshot":
            self = .snapshot(try SnapshotPayload(from: decoder))
        case "event":
            self = .event(try EventPayload(from: decoder))
        case "session_status":
            self = .sessionStatus(try SessionStatusPayload(from: decoder))
        case "approval_request":
            self = .approvalRequest(try ApprovalRequestPayload(from: decoder))
        case "pong":
            self = .pong
        default:
            // Invariant: unknown `t` → .unknown; never throw
            self = .unknown(t: t)
        }
    }
}

public struct HelloOkPayload: Codable, Sendable {
    public let serverVersion: String
    public let `protocol`: Int
    private enum CodingKeys: String, CodingKey {
        case serverVersion = "server_version"
        case `protocol`
    }
}

public struct ErrorPayload: Codable, Sendable {
    public let code: String
    public let message: String
}

public struct SessionListPayload: Codable, Sendable {
    public let sessions: [Session]
}

public struct SnapshotPayload: Codable, Sendable {
    public let sessionId: String
    public let session: Session
    public let events: [Event]
    public let cursorSeq: UInt64
    private enum CodingKeys: String, CodingKey {
        case session, events
        case sessionId  = "session_id"
        case cursorSeq  = "cursor_seq"
    }
}

public struct EventPayload: Codable, Sendable {
    public let sessionId: String
    public let event: Event
    private enum CodingKeys: String, CodingKey {
        case event
        case sessionId = "session_id"
    }
}

public struct SessionStatusPayload: Codable, Sendable {
    public let sessionId: String
    public let status: SessionStatus
    private enum CodingKeys: String, CodingKey {
        case status
        case sessionId = "session_id"
    }
}

public struct ApprovalRequestPayload: Codable, Sendable {
    public let approval: Approval
}

// MARK: - Client → server commands

/// Commands the iOS client sends to the daemon.
public enum ClientCommand: Encodable, Sendable {
    case hello(token: String)
    case listSessions
    case openSession(sessionId: String, fromSeq: UInt64)
    case sendPrompt(sessionId: String, text: String)
    case steer(sessionId: String, text: String)
    case approve(approvalId: String, decision: ApprovalDecision, reason: String?)
    case launchSession(agentType: String, repoPath: String, prompt: String?)
    case closeSession(sessionId: String)
    case ping

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: DynKey.self)
        switch self {
        case .hello(let token):
            try c.encode("hello", forKey: DynKey("t"))
            try c.encode(token, forKey: DynKey("token"))
            try c.encode(1, forKey: DynKey("protocol"))
        case .listSessions:
            try c.encode("list_sessions", forKey: DynKey("t"))
        case .openSession(let sid, let from):
            try c.encode("open_session", forKey: DynKey("t"))
            try c.encode(sid, forKey: DynKey("session_id"))
            try c.encode(from, forKey: DynKey("from_seq"))
        case .sendPrompt(let sid, let text):
            try c.encode("send_prompt", forKey: DynKey("t"))
            try c.encode(sid, forKey: DynKey("session_id"))
            try c.encode(text, forKey: DynKey("text"))
        case .steer(let sid, let text):
            try c.encode("steer", forKey: DynKey("t"))
            try c.encode(sid, forKey: DynKey("session_id"))
            try c.encode(text, forKey: DynKey("text"))
        case .approve(let aid, let decision, let reason):
            try c.encode("approve", forKey: DynKey("t"))
            try c.encode(aid, forKey: DynKey("approval_id"))
            try c.encode(decision.rawValue, forKey: DynKey("decision"))
            if let r = reason { try c.encode(r, forKey: DynKey("reason")) }
        case .launchSession(let agentType, let repo, let prompt):
            try c.encode("launch_session", forKey: DynKey("t"))
            try c.encode(agentType, forKey: DynKey("agent_type"))
            try c.encode(repo, forKey: DynKey("repo_path"))
            if let p = prompt { try c.encode(p, forKey: DynKey("prompt")) }
        case .closeSession(let sid):
            try c.encode("close_session", forKey: DynKey("t"))
            try c.encode(sid, forKey: DynKey("session_id"))
        case .ping:
            try c.encode("ping", forKey: DynKey("t"))
        }
    }
}

public enum ApprovalDecision: String, Sendable {
    case allow
    case deny
}

// Simple dynamic coding key
private struct DynKey: CodingKey {
    var stringValue: String
    var intValue: Int? { nil }
    init(_ s: String) { stringValue = s }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}
