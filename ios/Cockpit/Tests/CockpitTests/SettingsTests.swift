// CV5 — CockpitConfig + SettingsViewModel tests (ptyd config: host/port/token/fingerprint).
import Foundation
import Testing
@testable import Cockpit

// MARK: - Helpers

private func makeSuite() -> UserDefaults {
    UserDefaults(suiteName: "ai.rosslabs.cockpitTests.settings-\(UUID().uuidString)")!
}

private let validFP = String(repeating: "a", count: 64)

// MARK: - CockpitConfig round-trip

@Suite("CockpitConfig")
struct CockpitConfigTests {

    @Test("default host is 127.0.0.1")
    func defaultHost() {
        let cfg = CockpitConfig(defaults: makeSuite())
        #expect(cfg.host == "127.0.0.1")
    }

    @Test("default port string is 7333")
    func defaultPort() {
        let cfg = CockpitConfig(defaults: makeSuite())
        #expect(cfg.portString == "7333")
        #expect(cfg.port == 7333)
    }

    @Test("persists all four fields to UserDefaults")
    func roundTrip() {
        let suite = makeSuite()
        let cfg = CockpitConfig(defaults: suite)
        cfg.host = "tailscale.host"
        cfg.portString = "9000"
        cfg.pairingToken = "mytoken"
        cfg.pinnedFingerprint = validFP

        let cfg2 = CockpitConfig(defaults: suite)
        #expect(cfg2.host == "tailscale.host")
        #expect(cfg2.portString == "9000")
        #expect(cfg2.pairingToken == "mytoken")
        #expect(cfg2.pinnedFingerprint == validFP)
    }

    @Test("port is nil for non-numeric string")
    func invalidPortStringIsNil() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.portString = "notaport"
        #expect(cfg.port == nil)
    }

    @Test("port is nil for empty string")
    func emptyPortStringIsNil() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.portString = ""
        #expect(cfg.port == nil)
    }

    @Test("isConnectable false when host is empty")
    func notConnectableEmptyHost() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = ""
        cfg.portString = "7333"
        cfg.pairingToken = "tok"
        cfg.pinnedFingerprint = validFP
        #expect(!cfg.isConnectable)
    }

    @Test("isConnectable false when port invalid")
    func notConnectableInvalidPort() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.portString = "bad"
        cfg.pairingToken = "tok"
        cfg.pinnedFingerprint = validFP
        #expect(!cfg.isConnectable)
    }

    @Test("isConnectable false when token empty")
    func notConnectableEmptyToken() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.portString = "7333"
        cfg.pairingToken = ""
        cfg.pinnedFingerprint = validFP
        #expect(!cfg.isConnectable)
    }

    @Test("isConnectable false when fingerprint empty")
    func notConnectableEmptyFingerprint() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.portString = "7333"
        cfg.pairingToken = "tok"
        cfg.pinnedFingerprint = ""
        #expect(!cfg.isConnectable)
    }

    @Test("isConnectable true when all four fields valid")
    func connectableWithAll() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = "127.0.0.1"
        cfg.portString = "7333"
        cfg.pairingToken = "tok"
        cfg.pinnedFingerprint = validFP
        #expect(cfg.isConnectable)
    }
}

// MARK: - SettingsViewModel validation

@MainActor
@Suite("SettingsViewModel")
struct SettingsViewModelTests {

    private func makeVM(
        host: String = "127.0.0.1",
        port: String = "7333",
        token: String = "tok",
        fp: String = validFP
    ) -> SettingsViewModel {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = host
        cfg.portString = port
        cfg.pairingToken = token
        cfg.pinnedFingerprint = fp
        let vm = SettingsViewModel(config: cfg)
        vm.hostDraft = host
        vm.portDraft = port
        vm.tokenDraft = token
        vm.fingerprintDraft = fp
        return vm
    }

    @Test("valid config passes validation")
    func validConfig() {
        let vm = makeVM()
        #expect(vm.validate())
        #expect(vm.validationErrors.isEmpty)
    }

    @Test("empty host fails with emptyHost")
    func emptyHostFails() {
        let vm = makeVM(host: "")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.emptyHost))
    }

    @Test("invalid port fails with invalidPort")
    func invalidPortFails() {
        let vm = makeVM(port: "notaport")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.invalidPort))
    }

    @Test("empty port fails with emptyPort")
    func emptyPortFails() {
        let vm = makeVM(port: "")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.emptyPort))
    }

    @Test("empty token fails with emptyToken")
    func emptyTokenFails() {
        let vm = makeVM(token: "")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.emptyToken))
    }

    @Test("whitespace-only token fails")
    func whitespaceTokenFails() {
        let vm = makeVM(token: "   ")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.emptyToken))
    }

    @Test("short fingerprint fails with invalidFingerprint")
    func shortFingerprintFails() {
        let vm = makeVM(fp: "abc123")
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.invalidFingerprint))
    }

    @Test("non-hex fingerprint fails with invalidFingerprint")
    func nonHexFingerprintFails() {
        let vm = makeVM(fp: String(repeating: "z", count: 64))
        #expect(!vm.validate())
        #expect(vm.validationErrors.contains(.invalidFingerprint))
    }

    @Test("uppercase hex fingerprint passes")
    func upperHexFingerprintPasses() {
        let vm = makeVM(fp: String(repeating: "A", count: 64))
        #expect(vm.validate())
    }

    @Test("save persists trimmed values to config")
    func savePersists() {
        let cfg = CockpitConfig(defaults: makeSuite())
        let vm = SettingsViewModel(config: cfg)
        vm.hostDraft        = "  127.0.0.1  "
        vm.portDraft        = "  7333  "
        vm.tokenDraft       = "  newtoken  "
        vm.fingerprintDraft = "  \(validFP)  "
        let saved = vm.save()
        #expect(saved)
        #expect(cfg.host == "127.0.0.1")
        #expect(cfg.portString == "7333")
        #expect(cfg.pairingToken == "newtoken")
        #expect(cfg.pinnedFingerprint == validFP)
    }

    @Test("save with invalid config does not persist")
    func saveInvalidDoesNotPersist() {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = "original"
        let vm = SettingsViewModel(config: cfg)
        vm.hostDraft = ""
        let saved = vm.save()
        #expect(!saved)
        #expect(cfg.host == "original")
    }

    @Test("reset restores defaults and clears errors")
    func resetRestoresDefaults() {
        let vm = makeVM(host: "changed", port: "9999", token: "tok", fp: validFP)
        vm.reset()
        #expect(vm.hostDraft == CockpitConfig.defaultHost)
        #expect(vm.portDraft == String(CockpitConfig.defaultPort))
        #expect(vm.tokenDraft == "")
        #expect(vm.fingerprintDraft == "")
        #expect(vm.validationErrors.isEmpty)
    }
}
