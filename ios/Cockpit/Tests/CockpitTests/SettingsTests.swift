// G3 — SettingsViewModel validation + CockpitConfig UserDefaults round-trip tests.
import Foundation
import Testing
@testable import Cockpit

// MARK: - Helpers

private func makeSuite() -> UserDefaults {
    // Isolated UserDefaults suite — no pollution to .standard.
    let suite = UserDefaults(suiteName: "ai.rosslabs.cockpitTests.settings-\(UUID().uuidString)")!
    return suite
}

// MARK: - CockpitConfig round-trip

@Suite("CockpitConfig")
struct CockpitConfigTests {

    @Test("default URL is ws://127.0.0.1:8787")
    func defaultURL() {
        let cfg = CockpitConfig(defaults: makeSuite())
        #expect(cfg.daemonURLString == "ws://127.0.0.1:8787")
    }

    @Test("persists URL and token to UserDefaults")
    func roundTrip() {
        let suite = makeSuite()
        let cfg = CockpitConfig(defaults: suite)
        cfg.daemonURLString = "wss://example.tailscale/cockpit"
        cfg.devToken = "my-secret-token"

        // Read back via a second instance on the same suite.
        let cfg2 = CockpitConfig(defaults: suite)
        #expect(cfg2.daemonURLString == "wss://example.tailscale/cockpit")
        #expect(cfg2.devToken == "my-secret-token")
    }

    @Test("daemonURL is nil for http scheme")
    func httpSchemeIsInvalid() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "http://127.0.0.1:8787"
        #expect(cfg.daemonURL == nil)
    }

    @Test("daemonURL is nil for empty string")
    func emptyURLIsInvalid() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = ""
        #expect(cfg.daemonURL == nil)
    }

    @Test("daemonURL is non-nil for ws scheme")
    func wsSchemeIsValid() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "ws://127.0.0.1:8787"
        #expect(cfg.daemonURL != nil)
    }

    @Test("daemonURL is non-nil for wss scheme")
    func wssSchemeIsValid() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "wss://host.example.com/ws"
        #expect(cfg.daemonURL != nil)
    }

    @Test("isConnectable false when token is empty")
    func notConnectableWithEmptyToken() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "ws://127.0.0.1:8787"
        cfg.devToken = ""
        #expect(!cfg.isConnectable)
    }

    @Test("isConnectable true when URL and token are valid")
    func connectableWithBoth() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "ws://127.0.0.1:8787"
        cfg.devToken = "tok"
        #expect(cfg.isConnectable)
    }
}

// MARK: - SettingsViewModel validation

@MainActor
@Suite("SettingsViewModel")
struct SettingsViewModelTests {

    private func makeVM(url: String = "ws://127.0.0.1:8787", token: String = "tok") -> SettingsViewModel {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = url
        cfg.devToken = token
        return SettingsViewModel(config: cfg)
    }

    @Test("valid ws URL + non-empty token passes validation")
    func validWS() {
        let vm = makeVM(url: "ws://127.0.0.1:8787", token: "tok")
        let ok = vm.validate()
        #expect(ok)
        #expect(vm.validationErrors.isEmpty)
    }

    @Test("valid wss URL passes validation")
    func validWSS() {
        let vm = makeVM(url: "wss://cockpit.example.com/ws", token: "tok")
        #expect(vm.validate())
    }

    @Test("http URL is rejected with invalidURLScheme")
    func httpRejected() {
        let vm = makeVM(url: "http://127.0.0.1:8787")
        let ok = vm.validate()
        #expect(!ok)
        #expect(vm.validationErrors.contains(.invalidURLScheme))
    }

    @Test("empty URL produces emptyURL error")
    func emptyURL() {
        let vm = makeVM(url: "")
        let ok = vm.validate()
        #expect(!ok)
        #expect(vm.validationErrors.contains(.emptyURL))
    }

    @Test("empty token produces emptyToken error")
    func emptyToken() {
        let vm = makeVM(token: "")
        let ok = vm.validate()
        #expect(!ok)
        #expect(vm.validationErrors.contains(.emptyToken))
    }

    @Test("whitespace-only token produces emptyToken error")
    func whitespaceToken() {
        let vm = makeVM(token: "   ")
        let ok = vm.validate()
        #expect(!ok)
        #expect(vm.validationErrors.contains(.emptyToken))
    }

    @Test("save persists trimmed values to config")
    func savePersists() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "ws://127.0.0.1:8787"
        cfg.devToken = "old"
        let vm = SettingsViewModel(config: cfg)
        vm.urlDraft   = "  wss://new.host/ws  "
        vm.tokenDraft = "  new-token  "
        let saved = vm.save()
        #expect(saved)
        #expect(cfg.daemonURLString == "wss://new.host/ws")
        #expect(cfg.devToken == "new-token")
    }

    @Test("save with invalid URL does not persist")
    func saveInvalidDoesNotPersist() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.daemonURLString = "ws://127.0.0.1:8787"
        cfg.devToken = "orig"
        let vm = SettingsViewModel(config: cfg)
        vm.urlDraft = "http://bad"
        let saved = vm.save()
        #expect(!saved)
        #expect(cfg.daemonURLString == "ws://127.0.0.1:8787") // unchanged
    }

    @Test("reset restores default URL and clears token + errors")
    func resetRestoresDefaults() {
        let vm = makeVM(url: "wss://changed.host", token: "tok")
        vm.reset()
        #expect(vm.urlDraft == CockpitConfig.defaultDaemonURL)
        #expect(vm.tokenDraft == "")
        #expect(vm.validationErrors.isEmpty)
    }
}
