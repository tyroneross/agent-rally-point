// CV6-B — iCloud Keychain best-effort auto-fill unit tests.
//
// All tests use StubKeychainReader — no real keychain or entitlement required.
// TAG:UNTESTED (noted inline) — real synchronizable keychain read + cross-device
//   sync requires a signed build with a real Team ID + two same-Apple-ID devices.
//
// Spec invariants verified:
//   • valid blob   → attemptKeychainPairing returns true + applies payload fields
//   • nil read     → returns false + state → .needsManual (no crash, no spin)
//   • malformed    → returns false + state → .needsManual (no crash)
//   • keychain read alone does NOT reach .paired; .paired only after .connected

import Foundation
import Testing
import Combine
@testable import Cockpit

// MARK: - Test helpers

private let validToken = String(repeating: "c", count: 64)
private let validFP    = String(repeating: "d", count: 64)

private func makePayload(
    host: String = "10.0.0.1",
    port: Int    = 8443,
    token: String = validToken,
    fp: String    = validFP
) -> PairingPayload {
    PairingPayload(v: 1, host: host, port: port, token: token, fp: fp)
}

private func payloadData(_ p: PairingPayload) -> Data {
    p.jsonString()!.data(using: .utf8)!
}

private func makeSuite() -> UserDefaults {
    UserDefaults(suiteName: "ai.rosslabs.cockpitTests.keychain-\(UUID().uuidString)")!
}

// MARK: - StubKeychainReader

/// Controllable stub — replaces SecItemCopyMatching; no keychain, no entitlement needed.
struct StubKeychainReader: KeychainReading {
    let result: Data?
    func readPairingItem() -> Data? { result }
}

// MARK: - attemptKeychainPairing unit tests

@Suite("attemptKeychainPairing")
struct AttemptKeychainPairingTests {

    // 1. Valid payload blob → returns true + applies all four fields to config.
    @Test("valid payload blob → returns true + config fields applied")
    @MainActor
    func validPayload_appliesConfig() {
        let payload  = makePayload()
        let stub     = StubKeychainReader(result: payloadData(payload))
        let suite    = makeSuite()
        let config   = CockpitConfig(defaults: suite)
        var connectCalled = false

        let found = attemptKeychainPairing(reader: stub) { p in
            config.apply(p)
            connectCalled = true
        }

        #expect(found == true)
        #expect(connectCalled == true)
        #expect(config.host        == payload.host)
        #expect(config.portString  == String(payload.port))
        #expect(config.pairingToken == payload.token)
        #expect(config.pinnedFingerprint == payload.fp)
    }

    // 2. Reader returns nil → returns false; applyAndConnect is never called.
    @Test("nil read → returns false, applyAndConnect not called")
    @MainActor
    func nilRead_returnsFalse() {
        let stub = StubKeychainReader(result: nil)
        var applyCalled = false

        let found = attemptKeychainPairing(reader: stub) { _ in applyCalled = true }

        #expect(found == false)
        #expect(applyCalled == false)
    }

    // 3. Reader returns malformed bytes → returns false; no crash.
    @Test("malformed data → returns false, no crash")
    @MainActor
    func malformedData_returnsFalse() {
        let junk = Data([0xFF, 0xFE, 0x00, 0x01])
        let stub = StubKeychainReader(result: junk)
        var applyCalled = false

        let found = attemptKeychainPairing(reader: stub) { _ in applyCalled = true }

        #expect(found == false)
        #expect(applyCalled == false)
    }

    // 4. Reader returns valid UTF-8 but invalid JSON payload → returns false; no crash.
    @Test("valid UTF-8 but JSON that fails PairingPayload.decode → returns false")
    @MainActor
    func invalidJSON_returnsFalse() {
        // Bad version — will decode as JSON but fail the version check.
        let badJSON = #"{"v":99,"host":"x","port":8443,"token":"\#(validToken)","fp":"\#(validFP)"}"#
        let stub = StubKeychainReader(result: badJSON.data(using: .utf8)!)
        var applyCalled = false

        let found = attemptKeychainPairing(reader: stub) { _ in applyCalled = true }

        #expect(found == false)
        #expect(applyCalled == false)
    }
}

// MARK: - needsManualMessage

@Suite("needsManualMessage")
struct NeedsManualMessageTests {

    @Test("message is non-empty and mentions QR")
    func messageContent() {
        let msg = needsManualMessage()
        #expect(!msg.isEmpty)
        #expect(msg.contains("QR"))
    }
}

// MARK: - PairingCoordinatorState

@Suite("PairingCoordinatorState")
struct PairingCoordinatorStateTests {

    @Test(".unpaired is not surfaceable")
    func unpairedNotSurfaceable() {
        #expect(PairingCoordinatorState.unpaired.isSurfaceable == false)
    }

    @Test(".paired is not surfaceable")
    func pairedNotSurfaceable() {
        #expect(PairingCoordinatorState.paired.isSurfaceable == false)
    }

    @Test(".attempting is surfaceable")
    func attemptingSurfaceable() {
        #expect(PairingCoordinatorState.attempting(source: .keychain).isSurfaceable == true)
    }

    @Test(".verifying is surfaceable")
    func verifyingSurfaceable() {
        #expect(PairingCoordinatorState.verifying.isSurfaceable == true)
    }

    @Test(".needsManual is surfaceable")
    func needsManualSurfaceable() {
        #expect(PairingCoordinatorState.needsManual(reason: "test").isSurfaceable == true)
    }

    @Test(".needsManual carries the reason in statusLabel")
    func needsManualLabel() {
        let reason = "Scan QR please"
        let state = PairingCoordinatorState.needsManual(reason: reason)
        #expect(state.statusLabel == reason)
    }
}

// MARK: - PairingCoordinator state-machine tests

@Suite("PairingCoordinator state machine")
struct PairingCoordinatorStateMachineTests {

    // 5. nil reader → coordinator goes to .needsManual with the prompt message.
    @Test("nil keychain → coordinator reaches .needsManual with expected prompt")
    @MainActor
    func nilRead_goesToNeedsManual() async {
        let store = SessionStore()
        let stub  = StubKeychainReader(result: nil)
        let coord = PairingCoordinator(store: store, reader: stub)

        coord.attemptKeychain()

        // attemptKeychain() is synchronous up to the point of transition.
        if case .needsManual(let reason) = coord.state {
            #expect(reason == needsManualMessage())
        } else {
            Issue.record("Expected .needsManual, got \(coord.state)")
        }
    }

    // 6. Malformed data → coordinator reaches .needsManual; no crash.
    @Test("malformed data → coordinator reaches .needsManual, no crash")
    @MainActor
    func malformed_goesToNeedsManual() {
        let store = SessionStore()
        let stub  = StubKeychainReader(result: Data([0xDE, 0xAD]))
        let coord = PairingCoordinator(store: store, reader: stub)

        coord.attemptKeychain()

        if case .needsManual = coord.state {
            // pass
        } else {
            Issue.record("Expected .needsManual, got \(coord.state)")
        }
    }

    // 7. Keychain read alone does NOT produce .paired.
    //    After a successful read the coordinator moves to .verifying (not .paired).
    //    .paired only arrives after a simulated .connected transition.
    @Test("keychain read alone → .verifying, NOT .paired")
    @MainActor
    func keychainReadAlone_notPaired() {
        let payload = makePayload()
        let stub    = StubKeychainReader(result: payloadData(payload))
        let store   = SessionStore()
        let coord   = PairingCoordinator(store: store, reader: stub)

        coord.attemptKeychain()

        // After attemptKeychain, store.connect() was called internally.
        // The test harness has no live daemon, so connectionState stays .connecting.
        // The coordinator must be .verifying — NOT .paired.
        #expect(coord.state == .verifying)
        #expect(coord.state != .paired)
    }

    // 8. .paired is only reached after a simulated .connected (hello_ack OK).
    @Test(".paired only after simulated .connected (hello_ack round-trip)")
    @MainActor
    func pairedOnlyAfterConnected() async {
        let payload = makePayload()
        let stub    = StubKeychainReader(result: payloadData(payload))
        let store   = SessionStore()
        let coord   = PairingCoordinator(store: store, reader: stub)

        coord.attemptKeychain()
        // After read+apply: state == .verifying
        #expect(coord.state == .verifying)

        // Yield so the observer Task's first await on the publisher fires.
        for _ in 0..<10 { await Task.yield() }

        // Simulate a successful hello_ack by driving connectionState → .connected.
        store.transition(.connected)

        // Poll until the coordinator reacts (max ~0.5 s, fast on device).
        var iterations = 0
        while coord.state != .paired, iterations < 100 {
            await Task.yield()
            iterations += 1
        }

        #expect(coord.state == .paired,
                "coordinator must reach .paired only after connectionState becomes .connected")
    }

    // 9. Connection error → coordinator goes to .needsManual (not .paired).
    @Test("connection error → coordinator reaches .needsManual")
    @MainActor
    func connectionError_goesToNeedsManual() async {
        let payload = makePayload()
        let stub    = StubKeychainReader(result: payloadData(payload))
        let store   = SessionStore()
        let coord   = PairingCoordinator(store: store, reader: stub)

        coord.attemptKeychain()
        #expect(coord.state == .verifying)

        for _ in 0..<10 { await Task.yield() }

        store.transition(.error(message: "unauthorized: bad token"))

        var iterations = 0
        while coord.state == .verifying, iterations < 100 {
            await Task.yield()
            iterations += 1
        }

        if case .needsManual(let reason) = coord.state {
            #expect(reason.contains("unauthorized"))
        } else {
            Issue.record("Expected .needsManual after error, got \(coord.state)")
        }
    }
}
