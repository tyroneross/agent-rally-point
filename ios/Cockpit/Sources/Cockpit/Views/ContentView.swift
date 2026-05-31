// Root view — session list + navigation
import SwiftUI

public struct ContentView: View {
    @EnvironmentObject var store: SessionStore

    public init() {}

    public var body: some View {
        NavigationStack {
            SessionListView()
                .navigationTitle("Agent Cockpit")
                .toolbar {
                    ToolbarItem(placement: .navigationBarTrailing) {
                        NavigationLink("New") {
                            LauncherView()
                        }
                    }
                }
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
