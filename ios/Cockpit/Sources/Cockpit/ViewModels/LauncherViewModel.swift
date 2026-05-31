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
        do {
            let promptValue: String? = prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : prompt
            try await store.client.launchSession(agentType: selectedAgentType, repoPath: repo, prompt: promptValue)
        } catch {
            errorMessage = error.localizedDescription
        }
        isLaunching = false
    }
}
