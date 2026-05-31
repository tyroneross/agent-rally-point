// D3 — Launcher view-model (new session)
import Foundation

@MainActor
public final class LauncherViewModel: ObservableObject {
    @Published public var selectedAgentType: String = "claude"
    @Published public var repoPath: String = ""
    @Published public var prompt: String = ""
    @Published public var isLaunching: Bool = false
    @Published public var errorMessage: String? = nil

    public let agentTypes: [String] = ["claude", "codex"]

    private let store: SessionStore

    public init(store: SessionStore) {
        self.store = store
    }

    public func launch() async {
        let repo = repoPath.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !repo.isEmpty else {
            errorMessage = "Repo path is required."
            return
        }
        isLaunching = true
        errorMessage = nil
        // ptyd is an observe-model: it tails existing sessions; it does not spawn new ones.
        // "Launch" is a UI concept — the agent must already be running in a pane.
        // This stub fires pane.create (if a workspace is active), which starts a pane that
        // the caller can then attach an agent to via the shell prompt.
        // TAG:UNTESTED — requires workspace_id discovery + pane.create round-trip.
        let promptValue = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        var params: [String: Any] = [:]
        if !promptValue.isEmpty { params["command"] = ["bash", "-c", promptValue] }
        store.client.sendRawForUI(method: "pane.create", params: params)
        isLaunching = false
    }
}
