// CV5 — ConnectionState + SessionStore transition tests (updated for ptyd config).
import Foundation
import Testing
@testable import Cockpit

// MARK: - Helpers

private func makeSuite() -> UserDefaults {
    UserDefaults(suiteName: "ai.rosslabs.cockpitTests.connState-\(UUID().uuidString)")!
}

// MARK: - ConnectionState enum

@Suite("ConnectionState")
struct ConnectionStateEnumTests {

    @Test("idle label")
    func idleLabel() {
        #expect(ConnectionState.idle.label == "Not connected")
    }

    @Test("connecting label")
    func connectingLabel() {
        #expect(ConnectionState.connecting.label == "Connecting…")
    }

    @Test("connected label")
    func connectedLabel() {
        #expect(ConnectionState.connected.label == "Connected")
    }

    @Test("disconnected label")
    func disconnectedLabel() {
        #expect(ConnectionState.disconnected(reason: nil).label == "Disconnected")
    }

    @Test("error label")
    func errorLabel() {
        #expect(ConnectionState.error(message: "oops").label == "Connection error")
    }

    @Test("bannerMessage is nil for idle")
    func noBannerWhenIdle() {
        #expect(ConnectionState.idle.bannerMessage == nil)
    }

    @Test("bannerMessage is nil for connected")
    func noBannerWhenConnected() {
        #expect(ConnectionState.connected.bannerMessage == nil)
    }

    @Test("bannerMessage returns error message")
    func bannerMessageForError() {
        #expect(ConnectionState.error(message: "bad config").bannerMessage == "bad config")
    }

    @Test("bannerMessage returns disconnected reason")
    func bannerMessageForDisconnectedWithReason() {
        #expect(ConnectionState.disconnected(reason: "server closed").bannerMessage == "server closed")
    }

    @Test("bannerMessage nil for disconnected without reason")
    func noBannerForDisconnectedNoReason() {
        #expect(ConnectionState.disconnected(reason: nil).bannerMessage == nil)
    }

    @Test("needsBanner true for error")
    func needsBannerError() {
        #expect(ConnectionState.error(message: "x").needsBanner == true)
    }

    @Test("needsBanner false for connected")
    func noBannerConnected() {
        #expect(ConnectionState.connected.needsBanner == false)
    }

    @Test("equatable — same cases with same payloads are equal")
    func equalityPayloads() {
        #expect(ConnectionState.error(message: "a") == ConnectionState.error(message: "a"))
        #expect(ConnectionState.disconnected(reason: "r") == ConnectionState.disconnected(reason: "r"))
        #expect(ConnectionState.disconnected(reason: nil) == ConnectionState.disconnected(reason: nil))
    }

    @Test("equatable — different payloads are not equal")
    func inequalityPayloads() {
        #expect(ConnectionState.error(message: "a") != ConnectionState.error(message: "b"))
        #expect(ConnectionState.disconnected(reason: "x") != ConnectionState.disconnected(reason: "y"))
    }
}

// MARK: - SessionStore transition tests (ptyd config)

@MainActor
@Suite("SessionStore.connectionState transitions")
struct SessionStoreConnectionStateTests {

    private func makeStore(
        host: String = "127.0.0.1",
        port: String = "7333",
        token: String = "tok",
        fp: String = String(repeating: "a", count: 64)
    ) -> SessionStore {
        let cfg = CockpitConfig(defaults: makeSuite())
        cfg.host = host
        cfg.portString = port
        cfg.pairingToken = token
        cfg.pinnedFingerprint = fp
        return SessionStore(config: cfg)
    }

    @Test("starts in idle state")
    func startsIdle() {
        let store = makeStore()
        #expect(store.connectionState == .idle)
    }

    @Test("transition to connecting then connected")
    func idleToConnectingToConnected() {
        let store = makeStore()
        store.transition(.connecting)
        #expect(store.connectionState == .connecting)
        store.transition(.connected)
        #expect(store.connectionState == .connected)
    }

    @Test("empty host produces error state on connect()")
    func emptyHostProducesError() {
        let store = makeStore(host: "")
        store.connect()
        guard case .error(let msg) = store.connectionState else {
            Issue.record("Expected .error, got \(store.connectionState)")
            return
        }
        #expect(msg.lowercased().contains("host") || msg.lowercased().contains("empty"))
    }

    @Test("invalid port produces error state on connect()")
    func invalidPortProducesError() {
        let store = makeStore(port: "notaport")
        store.connect()
        guard case .error(let msg) = store.connectionState else {
            Issue.record("Expected .error, got \(store.connectionState)")
            return
        }
        #expect(msg.lowercased().contains("port"))
    }

    @Test("missing token produces error state on connect()")
    func missingTokenProducesError() {
        let store = makeStore(token: "")
        store.connect()
        guard case .error(let msg) = store.connectionState else {
            Issue.record("Expected .error, got \(store.connectionState)")
            return
        }
        #expect(msg.lowercased().contains("token"))
    }

    @Test("whitespace-only token produces error state on connect()")
    func whitespaceTokenProducesError() {
        let store = makeStore(token: "   ")
        store.connect()
        guard case .error = store.connectionState else {
            Issue.record("Expected .error, got \(store.connectionState)")
            return
        }
    }

    @Test("missing fingerprint produces error on connect()")
    func missingFingerprintProducesError() {
        let store = makeStore(fp: "")
        store.connect()
        guard case .error(let msg) = store.connectionState else {
            Issue.record("Expected .error, got \(store.connectionState)")
            return
        }
        #expect(msg.lowercased().contains("fingerprint") || msg.lowercased().contains("cert"))
    }

    @Test("connected then disconnected via transition")
    func connectedToDisconnected() {
        let store = makeStore()
        store.transition(.connected)
        store.transition(.disconnected(reason: "test closed"))
        guard case .disconnected(let r) = store.connectionState else {
            Issue.record("Expected .disconnected")
            return
        }
        #expect(r == "test closed")
    }

    @Test("disconnect() transitions to disconnected")
    func disconnectMethod() {
        let store = makeStore()
        store.transition(.connected)
        store.disconnect()
        guard case .disconnected = store.connectionState else {
            Issue.record("Expected .disconnected after disconnect()")
            return
        }
    }

    @Test("transition to error with a message")
    func transitionToError() {
        let store = makeStore()
        store.transition(.error(message: "unauthorized"))
        guard case .error(let msg) = store.connectionState else {
            Issue.record("Expected .error")
            return
        }
        #expect(msg == "unauthorized")
    }
}
