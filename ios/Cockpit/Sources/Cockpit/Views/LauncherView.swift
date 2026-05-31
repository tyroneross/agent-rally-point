// D3 — New session launcher
import SwiftUI

public struct LauncherView: View {
    @EnvironmentObject var store: SessionStore
    @StateObject private var vm: LauncherViewModel
    @Environment(\.dismiss) private var dismiss

    public init() {
        // vm initialized in .task via environment; placeholder here
        _vm = StateObject(wrappedValue: LauncherViewModel(store: SessionStore()))
    }

    public var body: some View {
        NavigationStack {
            Form {
                Section("Agent") {
                    Picker("Agent type", selection: $vm.selectedAgentType) {
                        ForEach(vm.agentTypes, id: \.self) { type in
                            Text(type).tag(type)
                        }
                    }
                    .pickerStyle(.segmented)
                }

                Section("Repository") {
                    TextField("/path/to/repo", text: $vm.repoPath)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                }

                Section("Initial prompt (optional)") {
                    TextEditor(text: $vm.prompt)
                        .frame(minHeight: 80)
                }

                if let err = vm.errorMessage {
                    Section {
                        Text(err).foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("New Session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if vm.isLaunching {
                        ProgressView()
                    } else {
                        Button("Launch") {
                            Task { await vm.launch(); if vm.errorMessage == nil { dismiss() } }
                        }
                        .disabled(vm.repoPath.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
            }
        }
    }
}
