// D2 — Session store / list view-model
// G3 — reads daemon URL + token from CockpitConfig (persisted UserDefaults).
import Foundation
import Combine

@MainActor
public final class SessionStore: ObservableObject {
    @Published public private(set) var sessions: [Session] = []
    @Published public var pendingApprovals: [Approval] = []

    public let client = CockpitClient()
    public let resync = ResyncMachine()
    public let config: CockpitConfig

    public init(config: CockpitConfig = CockpitConfig()) {
        self.config = config
        client.onFrame = { [weak self] frame in
            guard let self else { return }
            Task { @MainActor in
                self.handle(frame: frame)
            }
        }
    }

    // MARK: - Connect / disconnect

    public func connect() {
        guard let url = config.daemonURL else {
            // Config not valid — no-op; SettingsView guides user to fix it.
            return
        }
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

        case .helloOk, .pong, .error, .unknown:
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
