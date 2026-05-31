// G3 — Connection settings sheet (daemon URL + dev token).
import SwiftUI

public struct SettingsView: View {
    @StateObject private var vm: SettingsViewModel
    @Environment(\.dismiss) private var dismiss

    public init(config: CockpitConfig) {
        _vm = StateObject(wrappedValue: SettingsViewModel(config: config))
    }

    public var body: some View {
        NavigationStack {
            Form {
                Section {
                    LabeledContent("URL") {
                        TextField("ws://127.0.0.1:8787", text: $vm.urlDraft)
                            .keyboardType(.URL)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyURL) {
                        Text("URL is required.").font(.caption).foregroundStyle(.red)
                    } else if vm.validationErrors.contains(.invalidURLScheme) {
                        Text("URL must start with ws:// or wss://").font(.caption).foregroundStyle(.red)
                    } else if vm.validationErrors.contains(.malformedURL) {
                        Text("Invalid URL format.").font(.caption).foregroundStyle(.red)
                    }

                    LabeledContent("Dev Token") {
                        SecureField("required", text: $vm.tokenDraft)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyToken) {
                        Text("Token is required to connect.").font(.caption).foregroundStyle(.red)
                    }
                } header: {
                    Text("Daemon Connection")
                } footer: {
                    Text("ws:// for local/Tailscale; wss:// for TLS. Token is the dev bearer sent in the hello frame. Auth is dev-mode only — see DEFERRED.md for SE-mTLS.")
                        .font(.caption)
                }

                Section {
                    Button("Reset to Defaults", role: .destructive) {
                        vm.reset()
                    }
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        if vm.save() { dismiss() }
                    }
                }
            }
        }
    }
}
