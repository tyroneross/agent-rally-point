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

    // D3 — Send prompt via pane.send_text (ptyd path).
    // NOTE: requires a live pane_id for the session's agent pane — follow-up to
    // wire pane discovery. For now a no-op stub keeps the UI compiling.
    public func sendPrompt() async {
        let text = composerText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        composerText = ""
        // TAG:UNTESTED — pane_id discovery needed; ptyd path is pane.send_text.
        _ = store.client.sendRawForUI(method: "pane.send_text",
                                      params: ["pane_id": "TBD", "text": text])
    }

    // D3 — Steer (ptyd: pane.send_text). Same stub as sendPrompt.
    public func steer(text: String) async {
        // TAG:UNTESTED — pane_id discovery needed.
        _ = store.client.sendRawForUI(method: "pane.send_text",
                                      params: ["pane_id": "TBD", "text": text])
    }
}
