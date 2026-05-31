// D2 — Session detail view-model
import Foundation
import Combine

@MainActor
public final class SessionDetailViewModel: ObservableObject {
    public let session: Session
    @Published public private(set) var events: [Event] = []
    @Published public var composerText: String = ""

    private let store: SessionStore

    public init(session: Session, store: SessionStore) {
        self.session = session
        self.store = store
    }

    public func onAppear() {
        store.openSession(session)
        reload()
    }

    public func reload() {
        events = store.events(for: session.id)
    }

    // Called periodically or via Combine to refresh from resync
    public func refresh() {
        events = store.events(for: session.id)
    }

    // D3 — Send prompt
    public func sendPrompt() async {
        let text = composerText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        composerText = ""
        try? await store.client.sendPrompt(sessionId: session.id, text: text)
    }

    // D3 — Steer
    public func steer(text: String) async {
        try? await store.client.steer(sessionId: session.id, text: text)
    }
}
