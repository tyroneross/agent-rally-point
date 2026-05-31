// CV5 — Wire models aligned to the ptyd JSON-RPC protocol (CLIENT-API.md).
// Previous cockpit WebSocket wire shapes removed; ptyd shapes take over.
// Invariant: unknown `kind`, future result types MUST NOT throw on decode.

import Foundation

// MARK: - JSON value (free-form metadata)

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
        // Bool before Double — otherwise true/false parse as 1.0/0.0.
        if let v = try? c.decode(Bool.self)    { self = .bool(v);   return }
        if let v = try? c.decode(Double.self)  { self = .number(v); return }
        if let v = try? c.decode(String.self)  { self = .string(v); return }
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

public typealias JSONObject = [String: JSONValue]

// MARK: - ptyd Event (structured.rs shape)

/// Structured event emitted by `agent.subscribe_structured`.
/// Field names and types mirror ptyd's `Event` struct in structured.rs exactly:
///   - session_id: String (not UUID)
///   - created_at: UInt64 (Unix seconds, not a date string)
///   - kind: open String — unknown values decode without error
///   - metadata: free-form JSON object
public struct Event: Codable, Identifiable, Equatable, Sendable {
    public let sessionId: String
    public let seq: UInt64
    public let sender: String           // "agent" | "user" | "system"
    /// Open string — unknown kinds are kept as-is. Must never throw.
    public let kind: String
    public let content: String
    public let requiresUserInput: Bool
    /// Unix timestamp in seconds (u64 on the wire — matches ptyd's created_at: u64).
    public let createdAt: UInt64
    public let metadata: JSONObject

    public var id: String { "\(sessionId)-\(seq)" }

    private enum CodingKeys: String, CodingKey {
        case seq, sender, kind, content, metadata
        case sessionId         = "session_id"
        case requiresUserInput = "requires_user_input"
        case createdAt         = "created_at"
    }
}

// MARK: - ptyd AuditEntry (agent.get_audit)

/// One entry in the ptyd in-memory audit log.
public struct AuditEntry: Codable, Identifiable, Sendable {
    public let id: String
    public let ts: UInt64           // Unix seconds
    public let actor: String        // "client" | "system" | token id
    public let action: String       // e.g. "agent.approve", "approval_request_observed"
    public let sessionId: String?
    public let detail: JSONObject

    private enum CodingKeys: String, CodingKey {
        case id, ts, actor, action, detail
        case sessionId = "session_id"
    }
}

// MARK: - ptyd response envelopes

/// Top-level JSON-RPC success envelope: `{"id":..., "result":{"type":...,...}}`.
public struct PtydResponse: Decodable {
    public let id: String
    public let result: PtydResult?
    public let error: PtydError?
}

/// Discriminated result — type-tagged.
/// Unknown `type` values decode to `.unknown(type, rawData)`.
public enum PtydResult: Sendable {
    case helloAck(HelloAckResult)
    case structuredSubscriptionStarted(sessionId: String)
    case auditList([AuditEntry])
    case approved(approvalId: String, decision: String)
    case ok
    case unknown(type: String)
}

extension PtydResult: Decodable {
    private enum TypeKey: String, CodingKey { case type }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: TypeKey.self)
        let type_ = try container.decode(String.self, forKey: .type)
        switch type_ {
        case "hello_ack":
            self = .helloAck(try HelloAckResult(from: decoder))
        case "structured_subscription_started":
            let c = try decoder.container(keyedBy: SubStartedKeys.self)
            let sid = try c.decode(String.self, forKey: .sessionId)
            self = .structuredSubscriptionStarted(sessionId: sid)
        case "audit_list":
            let c = try decoder.container(keyedBy: AuditListKeys.self)
            let entries = try c.decode([AuditEntry].self, forKey: .entries)
            self = .auditList(entries)
        case "approved":
            let c = try decoder.container(keyedBy: ApprovedKeys.self)
            let aid = try c.decode(String.self, forKey: .approvalId)
            let dec = try c.decode(String.self, forKey: .decision)
            self = .approved(approvalId: aid, decision: dec)
        case "ok":
            self = .ok
        default:
            self = .unknown(type: type_)
        }
    }

    private enum SubStartedKeys: String, CodingKey { case sessionId = "session_id" }
    private enum AuditListKeys: String, CodingKey  { case entries }
    private enum ApprovedKeys: String, CodingKey   {
        case approvalId = "approval_id"
        case decision
    }
}

/// hello_ack result payload — matches mobile.rs HelloAck.
public struct HelloAckResult: Decodable, Sendable {
    public let ok: Bool
    public let protocolVersion: Int
    public let error: String?
    public let capabilities: [String]

    private enum CodingKeys: String, CodingKey {
        case ok, error, capabilities
        case protocolVersion = "protocol_version"
    }
}

/// JSON-RPC error object.
public struct PtydError: Decodable, Sendable {
    public let code: String
    public let message: String
}

// MARK: - ApprovalDecision

public enum ApprovalDecision: String, Sendable {
    case allow
    case deny
}

// MARK: - Session / Approval (retained from cockpit — used by UI)

/// Session domain object (used by SessionStore + list UI).
public struct Session: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let ownerId: String
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

/// Approval domain object (used by ApprovalView).
public struct Approval: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let sessionId: String
    public let eventSeq: UInt64
    public let tool: String
    public let args: JSONObject
    public let createdAt: String
    public let ttlSecs: UInt64
    public let resolution: String?

    private enum CodingKeys: String, CodingKey {
        case id, tool, args, resolution
        case sessionId  = "session_id"
        case eventSeq   = "event_seq"
        case createdAt  = "created_at"
        case ttlSecs    = "ttl_secs"
    }
}
