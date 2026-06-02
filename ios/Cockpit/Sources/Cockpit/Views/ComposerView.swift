// D3 — Composer: send prompt or steer mid-run
import SwiftUI

struct ComposerView: View {
    @ObservedObject var vm: SessionDetailViewModel
    @State private var isSteer = false

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            VStack(alignment: .leading, spacing: 4) {
                Toggle("Steer", isOn: $isSteer)
                    .toggleStyle(.button)
                    .font(.caption)
                    .tint(.orange)

                TextField(isSteer ? "Steering message…" : "Send a prompt…", text: $vm.composerText, axis: .vertical)
                    .lineLimit(1...5)
                    .textFieldStyle(.roundedBorder)
            }

            Button {
                Task {
                    if isSteer {
                        let text = vm.composerText.trimmingCharacters(in: .whitespacesAndNewlines)
                        guard !text.isEmpty else { return }
                        vm.composerText = ""
                        await vm.steer(text: text)
                    } else {
                        await vm.sendPrompt()
                    }
                }
            } label: {
                Image(systemName: "arrow.up.circle.fill")
                    .font(.title2)
                    .foregroundStyle(vm.composerText.isEmpty ? Color.secondary : Color.accentColor)
            }
            .disabled(vm.composerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
    }
}
