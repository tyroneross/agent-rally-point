// Root view — session list + navigation
// G3 — settings sheet added (gear toolbar button, leading side).
// G4 — compact connection-state indicator + error/disconnected banner (text color only, no background badges).
// CV6-B — iCloud Keychain best-effort auto-fill status row (attempting/verifying/needsManual).
import SwiftUI

public struct ContentView: View {
    @EnvironmentObject var store: SessionStore
    /// CV6-B — optional; ContentView degrades gracefully when coordinator is absent (previews).
    @EnvironmentObject var pairingCoordinator: PairingCoordinator
    @State private var showSettings = false

    public init() {}

    public var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                // CV6-B — keychain pairing status row. Visible only during attempting/verifying/needsManual.
                // Text-color only; no background badge (Calm Precision rule).
                KeychainPairingStatusView(
                    state: pairingCoordinator.state,
                    showSettings: $showSettings
                )

                // Error / disconnected banner — visible only when there's a reason to surface.
                ConnectionBannerView(state: store.connectionState, showSettings: $showSettings)

                SessionListView()
            }
            .navigationTitle("Agent Cockpit")
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) {
                    Button {
                        showSettings = true
                    } label: {
                        Label("Settings", systemImage: "gear")
                    }
                }
                ToolbarItem(placement: .navigationBarTrailing) {
                    HStack(spacing: 8) {
                        ConnectionStatusIndicator(state: store.connectionState)
                        NavigationLink("New") {
                            LauncherView()
                        }
                    }
                }
            }
        }
        .sheet(isPresented: $showSettings) {
            SettingsView(config: store.config)
        }
        .task {
            // CV6-B: if config has no token yet, try iCloud Keychain auto-fill first.
            // If the item is absent or undecodable the coordinator moves to .needsManual
            // immediately and the existing QR/manual Settings UI remains available.
            // If the item is present, coordinator calls config.apply + store.connect
            // internally — do NOT call store.connect() again here.
            if store.config.pairingToken.trimmingCharacters(in: .whitespaces).isEmpty {
                pairingCoordinator.attemptKeychain()
            } else {
                store.connect()
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didBecomeActiveNotification)) { _ in
            let ids = store.sessions.map(\.id)
            store.handleForeground(sessionIds: ids)
        }
    }
}

// MARK: - CV6-B: Keychain pairing status row

/// Shown only during `.attempting`, `.verifying`, or `.needsManual`.
/// Invisible for `.unpaired` and `.paired` — zero footprint when not surfaceable.
/// Text-color only; no background badge (Calm Precision rule).
struct KeychainPairingStatusView: View {
    let state: PairingCoordinatorState
    @Binding var showSettings: Bool

    var body: some View {
        if state.isSurfaceable {
            HStack(spacing: 8) {
                Image(systemName: iconName)
                    .imageScale(.small)
                    .foregroundStyle(statusColor)
                Text(state.statusLabel)
                    .font(.footnote)
                    .foregroundStyle(.primary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if case .needsManual = state {
                    Button("Settings") { showSettings = true }
                        .font(.footnote.bold())
                        .foregroundStyle(.blue)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color(.systemBackground))
            .overlay(alignment: .bottom) { Divider() }
        }
    }

    private var iconName: String {
        switch state {
        case .attempting, .verifying: return "key.icloud"
        case .needsManual:            return "exclamationmark.triangle"
        default:                      return "key.icloud"
        }
    }

    private var statusColor: Color {
        switch state {
        case .attempting:  return .orange
        case .verifying:   return .orange
        case .needsManual: return .red
        default:           return .secondary
        }
    }
}

// MARK: - Status indicator (text color only — Calm Precision: no background badge)

/// One-word connection status in the nav bar — color carries meaning, no capsule/badge.
struct ConnectionStatusIndicator: View {
    let state: ConnectionState

    var body: some View {
        Text(state.label)
            .font(.caption2)
            .foregroundStyle(indicatorColor)
    }

    private var indicatorColor: Color {
        switch state {
        case .idle:          return .secondary
        case .connecting:    return .orange
        case .connected:     return .green
        case .disconnected:  return .secondary
        case .error:         return .red
        }
    }
}

// MARK: - Error / disconnected banner

/// Inline banner shown only when .error or .disconnected(reason:) — otherwise renders nothing.
struct ConnectionBannerView: View {
    let state: ConnectionState
    @Binding var showSettings: Bool

    var body: some View {
        if let msg = state.bannerMessage {
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .imageScale(.small)
                    .foregroundStyle(.red)
                Text(msg)
                    .font(.footnote)
                    .foregroundStyle(.primary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button("Settings") {
                    showSettings = true
                }
                .font(.footnote.bold())
                .foregroundStyle(.blue)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color(.systemBackground))
            .overlay(alignment: .bottom) {
                Divider()
            }
        }
    }
}
