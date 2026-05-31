// Root view — session list + navigation
// G3 — settings sheet added (gear toolbar button, leading side).
import SwiftUI

public struct ContentView: View {
    @EnvironmentObject var store: SessionStore
    @State private var showSettings = false

    public init() {}

    public var body: some View {
        NavigationStack {
            SessionListView()
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
                        NavigationLink("New") {
                            LauncherView()
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
