// D2 — Session store / list view-model
// G3 — reads daemon URL + token from CockpitConfig (persisted UserDefaults).
// G4 — exposes ConnectionState; surfaces config errors instead of silently no-opping.
import Foundation
import Combine

@MainActor
public final class SessionStore: ObservableObject {
    @Published public private(set) var sessions: [Session] = []
    @Published public var pendingApprovals: [Approval] = []

    /// Store-level connection lifecycle state. Views bind here; unit-testable via transition(_:).
    @Published public private(set) var connectionState: ConnectionState = .idle

    public let client = CockpitClient()
    public let resync = ResyncMachine()
    public let config: CockpitConfig
    private var cancellables: Set<AnyCancellable> = []

    public init(config: CockpitConfig = CockpitConfig()) {
        self.config = config
        client.onFrame = { [weak self] frame in
            guard let self else { return }
            Task { @MainActor in
                self.handle(frame: frame)
            }
        }
        // Mirror CockpitClient socket failures into our state.
        client.$connectionState
            .receive(on: RunLoop.main)
            .sink { [weak self] clientState in
                guard let self else { return }
                switch clientState {
                case .failed(let msg):
                    self.transition(.error(message: msg))
                case .disconnected:
                    // Only update if we were connected (avoids clobbering a config error).
                    if case .connected = self.connectionState {
                        self.transition(.disconnected(reason: nil))
                    }
                default:
                    break
                }
            }
            .store(in: &cancellables)
    }

    // MARK: - State machine

    /// Applies a transition. Isolated so unit tests can drive it without a live socket.
    public func transition(_ next: ConnectionState) {
        connectionState = next
    }

    // MARK: - Connect / disconnect

    public func connect() {
        guard let url = config.daemonURL else {
            let reason: String
            if config.daemonURLString.trimmingCharacters(in: .whitespaces).isEmpty {
                reason = "Daemon URL is empty. Open Settings to configure."
            } else {
                reason = "Daemon URL \"\(config.daemonURLString)\" is invalid. Use ws:// or wss://."
            }
            transition(.error(message: reason))
            return
        }
        guard !config.devToken.trimmingCharacters(in: .whitespaces).isEmpty else {
            transition(.error(message: "Dev token is missing. Open Settings to add one."))
            return
        }
        transition(.connecting)
        let token = config.devToken
        client.connect(to: url, token: token)
        // After connect, list sessions
        Task {
            try? await Task.sleep(nanoseconds: 300_000_000)  // wait for hello_ok
            try? await client.sendCommand(.listSessions)
        }
    }

    public func disconnect() {
        client.disconnect()
        transition(.disconnected(reason: nil))
    }

    // MARK: - Session detail subscription

    public func openSession(_ session: Session) {
        let fromSeq = resync.fromSeq(for: session.id)
        Task {
            try? await client.openSession(id: session.id, fromSeq: fromSeq)
        }
    }

    public func events(for sessionId: String) -> [Event] {
        resync.events(for: sessionId)
    }

    // MARK: - Frame handling

    private func handle(frame: ServerFrame) {
        switch frame {
        case .sessionList(let p):
            sessions = p.sessions

        case .snapshot(let p):
            resync.applySnapshot(sessionId: p.sessionId, events: p.events, cursorSeq: p.cursorSeq)
            // Update session record
            if let idx = sessions.firstIndex(where: { $0.id == p.sessionId }) {
                sessions[idx] = p.session
            } else {
                sessions.append(p.session)
            }

        case .event(let p):
            resync.applyDelta(sessionId: p.sessionId, event: p.event)

        case .sessionStatus(let p):
            if let idx = sessions.firstIndex(where: { $0.id == p.sessionId }) {
                let s = sessions[idx]
                sessions[idx] = Session(
                    id: s.id, ownerId: s.ownerId, agentType: s.agentType,
                    repoPath: s.repoPath, status: p.status, title: s.title,
                    createdAt: s.createdAt, lastSeq: s.lastSeq
                )
            }

        case .approvalRequest(let p):
            if !pendingApprovals.contains(where: { $0.id == p.approval.id }) {
                pendingApprovals.append(p.approval)
            }

        case .helloOk:
            transition(.connected)

        case .error(let p):
            transition(.error(message: p.message.isEmpty ? p.code : p.message))

        case .pong, .unknown:
            break
        }
    }

    // MARK: - Background/foreground (D4)

    public func handleForeground(sessionIds: [String]) {
        for sid in sessionIds {
            resync.handleBackground(sessionId: sid)  // noop cursor-preserve
            let fromSeq = resync.fromSeq(for: sid)
            Task {
                try? await client.openSession(id: sid, fromSeq: fromSeq)
            }
        }
    }
}
