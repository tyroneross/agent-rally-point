// CV5 — Session store re-wired to ptyd TLS thin client.
// Drives CockpitClient (NWConnection + TLS + cert pinning) rather than URLSessionWebSocketTask.
// Parses ptyd JSON-RPC line stream: hello_ack → connected; Event stream → resync.
// ConnectionState mirrors the ptyd connection lifecycle (idle → connecting → connected → error).
import Foundation
import Combine

@MainActor
public final class SessionStore: ObservableObject {
    @Published public private(set) var sessions: [Session] = []
    @Published public var pendingApprovals: [Approval] = []
    @Published public private(set) var connectionState: ConnectionState = .idle

    /// Per-session event store + cursor tracking (retained from CV2).
    public let resync = ResyncMachine()

    /// ptyd TLS client. Public so view-models can access it for pane.send_text etc.
    public let client = CockpitClient()

    public let config: CockpitConfig
    private var cancellables: Set<AnyCancellable> = []
    private let decoder = JSONDecoder()

    public init(config: CockpitConfig = CockpitConfig()) {
        self.config = config

        // Mirror CockpitClient socket state into SessionStore state.
        // hello_ack success is handled in handleLine; here we only mirror
        // transport-level failures and clean disconnections.
        client.$connectionState
            .receive(on: RunLoop.main)
            .sink { [weak self] clientState in
                guard let self else { return }
                switch clientState {
                case .error(let msg):
                    self.transition(.error(message: msg))
                case .disconnected:
                    if case .connected = self.connectionState {
                        self.transition(.disconnected(reason: nil))
                    }
                case .idle:
                    if case .connecting = self.connectionState { } // don't clobber
                default:
                    break
                }
            }
            .store(in: &cancellables)

        // Wire raw JSON lines from the client into our line handler.
        client.onLine = { [weak self] data in
            guard let self else { return }
            Task { @MainActor in
                self.handleLine(data)
            }
        }
    }

    // MARK: - State machine

    public func transition(_ next: ConnectionState) {
        connectionState = next
    }

    // MARK: - Connect / disconnect

    public func connect() {
        let h = config.host.trimmingCharacters(in: .whitespaces)
        guard !h.isEmpty else {
            transition(.error(message: "Host is empty. Open Settings to configure."))
            return
        }
        guard let port = config.port else {
            transition(.error(message: "Port \"\(config.portString)\" is invalid."))
            return
        }
        let token = config.pairingToken.trimmingCharacters(in: .whitespaces)
        guard !token.isEmpty else {
            transition(.error(message: "Pairing token is missing. Open Settings to add one."))
            return
        }
        let fp = config.pinnedFingerprint.trimmingCharacters(in: .whitespaces)
        guard !fp.isEmpty else {
            transition(.error(message: "Cert fingerprint is missing. Open Settings to add one."))
            return
        }
        transition(.connecting)
        client.connect(host: h, port: port, pairingToken: token, pinnedFingerprint: fp)
    }

    public func disconnect() {
        client.disconnect()
        transition(.disconnected(reason: nil))
    }

    // MARK: - Session subscription

    public func openSession(_ session: Session) {
        // Map to ptyd agent.subscribe_structured.
        // agent + base_dir are inferred from agentType; from = resume cursor.
        let (agent, baseDir) = agentMeta(for: session.agentType)
        let from = Int(resync.fromSeq(for: session.id))
        client.subscribeStructured(
            sessionID: session.id,
            agent: agent,
            baseDir: baseDir,
            from: from > 0 ? from : nil
        )
    }

    public func events(for sessionId: String) -> [Event] {
        resync.events(for: sessionId)
    }

    // MARK: - Line handler (ptyd JSON-RPC stream)

    private func handleLine(_ data: Data) {
        // Lines from the server are either:
        //   (a) A JSON-RPC response: {"id":..., "result":{"type":...}} or {"id":..., "error":{...}}
        //   (b) A naked Event JSON object emitted by agent.subscribe_structured after the ack.

        // Try response envelope first.
        if let response = try? decoder.decode(PtydResponse.self, from: data) {
            handleResponse(response)
            return
        }

        // Try naked Event (the structured subscription stream).
        if let event = try? decoder.decode(Event.self, from: data) {
            resync.applyDelta(sessionId: event.sessionId, event: event)
            // If the event is an approval_request, surface it.
            if event.requiresUserInput,
               let approvalId = event.metadata["approval_id"].flatMap({ if case .string(let s) = $0 { s } else { nil } }) {
                let tool: String
                if case .string(let t) = event.metadata["tool"] { tool = t } else { tool = "unknown" }
                // Build a lightweight Approval from the event metadata for the UI.
                let approval = Approval(
                    id: approvalId,
                    sessionId: event.sessionId,
                    eventSeq: event.seq,
                    tool: tool,
                    args: (event.metadata["args"].flatMap { if case .object(let o) = $0 { o } else { nil } }) ?? [:],
                    createdAt: String(event.createdAt),
                    ttlSecs: 120,
                    resolution: nil
                )
                if !pendingApprovals.contains(where: { $0.id == approval.id }) {
                    pendingApprovals.append(approval)
                }
            }
            return
        }

        // Unrecognized line — ignore silently.
    }

    private func handleResponse(_ response: PtydResponse) {
        if let error = response.error {
            // Auth errors on hello → surface as .error state.
            // Other errors → log only (don't disconnect).
            if error.code == "unauthorized" || error.code == "bad_request" {
                transition(.error(message: "\(error.code): \(error.message)"))
            }
            return
        }
        guard let result = response.result else { return }
        switch result {
        case .helloAck(let ack):
            if ack.ok {
                transition(.connected)
            } else {
                let msg = ack.error ?? "hello rejected (unknown reason)"
                transition(.error(message: "Auth failed: \(msg)"))
            }
        case .structuredSubscriptionStarted:
            // Ack received — stream is now active; no extra state transition needed.
            break
        case .auditList:
            // Handled by direct callers via onLine if they need the entries.
            break
        case .approved:
            // Audit recorded on daemon side; remove from pending if present.
            break
        case .ok, .unknown:
            break
        }
    }

    // MARK: - Background/foreground

    public func handleForeground(sessionIds: [String]) {
        for sid in sessionIds {
            resync.handleBackground(sessionId: sid)
            if let session = sessions.first(where: { $0.id == sid }) {
                openSession(session)
            }
        }
    }

    // MARK: - Helpers

    /// Maps an agentType string to (ptyd agent param, base_dir).
    private func agentMeta(for agentType: String) -> (String, String) {
        switch agentType.lowercased() {
        case "codex":
            return ("codex", "~/.codex/sessions")
        default:
            return ("claude", "~/.claude/projects")
        }
    }
}
