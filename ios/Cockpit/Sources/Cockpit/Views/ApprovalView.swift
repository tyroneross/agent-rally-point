// D3 — Approval prompt UI
// TAG:UNTESTED Face ID gate requires a physical device; dev mode always allows.
import SwiftUI

public struct ApprovalBannerView: View {
    @ObservedObject var vm: ApprovalViewModel
    @State private var denyReason: String = ""

    public init(vm: ApprovalViewModel) {
        self.vm = vm
    }

    public var body: some View {
        if let approval = vm.pendingApprovals.first {
            VStack(alignment: .leading, spacing: 12) {
                Label("Tool approval required", systemImage: "shield.lefthalf.filled")
                    .font(.headline)
                    .foregroundStyle(.orange)

                VStack(alignment: .leading, spacing: 4) {
                    Text("Tool: \(approval.tool)")
                        .font(.subheadline.weight(.semibold))
                    Text("Session: \(approval.sessionId.prefix(8))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                TextField("Deny reason (optional)", text: $denyReason)
                    .textFieldStyle(.roundedBorder)

                HStack {
                    Button(role: .destructive) {
                        Task { await vm.deny(approval, reason: denyReason.isEmpty ? nil : denyReason) }
                    } label: {
                        Label("Deny", systemImage: "xmark.circle")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.bordered)

                    Button {
                        Task { await vm.allow(approval) }
                    } label: {
                        Label("Allow", systemImage: "checkmark.circle")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .tint(.green)
                }
            }
            .padding()
            .background(.regularMaterial)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .padding()
        }
    }
}
