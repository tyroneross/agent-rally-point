// Root view — session list + navigation
// G3 — settings sheet added (gear toolbar button, leading side).
// G4 — compact connection-state indicator + error/disconnected banner (text color only, no background badges).
import SwiftUI

public struct ContentView: View {
    @EnvironmentObject var store: SessionStore
    @State private var showSettings = false

    public init() {}

    public var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
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
            store.connect()
        }
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didBecomeActiveNotification)) { _ in
            let ids = store.sessions.map(\.id)
            store.handleForeground(sessionIds: ids)
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
