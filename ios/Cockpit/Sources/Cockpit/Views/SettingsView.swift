// CV5 — Connection settings sheet for ptyd TLS thin client.
// Fields: host, port, pairing token, pinned cert fingerprint.
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
                    LabeledContent("Host") {
                        TextField("127.0.0.1", text: $vm.hostDraft)
                            .keyboardType(.URL)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyHost) {
                        Text("Host is required.").font(.caption).foregroundStyle(.red)
                    }

                    LabeledContent("Port") {
                        TextField("7333", text: $vm.portDraft)
                            .keyboardType(.numberPad)
                            .autocorrectionDisabled()
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyPort) {
                        Text("Port is required.").font(.caption).foregroundStyle(.red)
                    } else if vm.validationErrors.contains(.invalidPort) {
                        Text("Port must be 1–65535.").font(.caption).foregroundStyle(.red)
                    }
                } header: {
                    Text("Daemon Address")
                } footer: {
                    Text("The ptyd TLS listener address. The daemon binds loopback only — reach it via your Tailscale/SSH tunnel.")
                        .font(.caption)
                }

                Section {
                    LabeledContent("Pairing Token") {
                        SecureField("64-char hex token", text: $vm.tokenDraft)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyToken) {
                        Text("Pairing token is required.").font(.caption).foregroundStyle(.red)
                    }
                } header: {
                    Text("Authentication")
                } footer: {
                    Text("Token from ~/.config/ptyd/pairing_token on the daemon host. Share via QR code or secure copy.")
                        .font(.caption)
                }

                Section {
                    LabeledContent("Cert Fingerprint") {
                        TextField("SHA-256 hex (64 chars)", text: $vm.fingerprintDraft)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .multilineTextAlignment(.trailing)
                    }
                    if vm.validationErrors.contains(.emptyFingerprint) {
                        Text("Fingerprint is required.").font(.caption).foregroundStyle(.red)
                    } else if vm.validationErrors.contains(.invalidFingerprint) {
                        Text("Must be a 64-character hex string (SHA-256).").font(.caption).foregroundStyle(.red)
                    }
                } header: {
                    Text("Certificate Pinning")
                } footer: {
                    Text("SHA-256 fingerprint of the daemon's self-signed TLS cert. Shown on daemon startup and in `ptyd status server --json` as tls_fingerprint. This client will reject any other cert.")
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
