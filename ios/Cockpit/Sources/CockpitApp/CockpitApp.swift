import SwiftUI

@main
struct CockpitApp: App {
    @StateObject private var store = SessionStore()
    // CV6-B — iCloud Keychain pairing coordinator. Injected as environment object
    // so ContentView and SettingsView can bind to coordinator.state for status display.
    @StateObject private var pairingCoordinator: PairingCoordinator

    init() {
        let s = SessionStore()
        _store = StateObject(wrappedValue: s)
        _pairingCoordinator = StateObject(
            wrappedValue: PairingCoordinator(store: s, reader: SyncedKeychainReader())
        )
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(store)
                .environmentObject(pairingCoordinator)
        }
    }
}
