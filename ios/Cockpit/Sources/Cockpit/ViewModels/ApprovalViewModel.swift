// D3 — Approval view-model
// TAG:UNTESTED Face ID requires a physical device; stubbed behind devMode flag.
import Foundation

/// Controls the approval prompt lifecycle.
@MainActor
public final class ApprovalViewModel: ObservableObject {
    @Published public var pendingApprovals: [Approval] = []

    /// When true, Face ID gate is bypassed (dev/simulator mode).
    public static let devMode: Bool = {
        #if targetEnvironment(simulator)
        return true
        #else
        return ProcessInfo.processInfo.environment["COCKPIT_DEV_MODE"] == "1"
        #endif
    }()

    private let store: SessionStore

    public init(store: SessionStore) {
        self.store = store
    }

    // MARK: - Resolve

    public func allow(_ approval: Approval) async {
        guard await authenticateIfNeeded() else { return }
        try? await store.client.approve(approvalId: approval.id, decision: .allow)
        remove(approval)
    }

    public func deny(_ approval: Approval, reason: String? = nil) async {
        try? await store.client.approve(approvalId: approval.id, decision: .deny, reason: reason)
        remove(approval)
    }

    private func remove(_ approval: Approval) {
        pendingApprovals.removeAll { $0.id == approval.id }
        store.pendingApprovals.removeAll { $0.id == approval.id }
    }

    // MARK: - Face ID (stubbed)

    /// Returns true if the action is authorized.
    /// On simulator/dev mode: always returns true without prompting.
    /// On device with devMode=false: would use LocalAuthentication (TAG:UNTESTED).
    private func authenticateIfNeeded() async -> Bool {
        if Self.devMode {
            // TAG:UNTESTED Face ID requires a physical device — bypassed in simulator
            return true
        }
        // TAG:UNTESTED LocalAuthentication path — physical device only
        // let context = LAContext()
        // return (try? await context.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: "Authorize tool approval")) ?? false
        return true
    }
}
