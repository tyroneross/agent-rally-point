// CV6-A — Pairing payload + QR round-trip + CockpitConfig.apply tests.
// TAG:UNTESTED — camera capture path requires a physical device with a camera.
// The QR-image round-trip (makeQRImage → decodeQR → decode) covers the real encode/decode
// path end-to-end without any hardware.

import Foundation
import Testing
@testable import Cockpit

// MARK: - Helpers

private func makeSuite() -> UserDefaults {
    UserDefaults(suiteName: "ai.rosslabs.cockpitTests.pairing-\(UUID().uuidString)")!
}

private let validToken = String(repeating: "a", count: 64)
private let validFP    = String(repeating: "b", count: 64)

private func validPayload(
    v: Int    = 1,
    host: String = "100.1.2.3",
    port: Int = 8443,
    token: String = validToken,
    fp: String    = validFP
) -> PairingPayload {
    PairingPayload(v: v, host: host, port: port, token: token, fp: fp)
}

// MARK: - PairingPayload encode/decode round-trip

@Suite("PairingPayload")
struct PairingPayloadTests {

    @Test("encode→decode round-trip equals original")
    func jsonRoundTrip() throws {
        let p = validPayload()
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        switch result {
        case .success(let decoded): #expect(decoded == p)
        case .failure(let err): Issue.record("Unexpected failure: \(err)")
        }
    }

    @Test("malformed JSON returns .malformedJSON")
    func malformedJSON() {
        let result = PairingPayload.decode(fromQRString: "not json at all")
        #expect(result == .failure(.malformedJSON))
    }

    @Test("bad version (v:2) returns .badVersion(2)")
    func badVersion() throws {
        let p = validPayload(v: 2)
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.badVersion(2)))
    }

    @Test("short token (63 hex chars) returns .invalidField(token)")
    func shortToken() throws {
        let p = validPayload(token: String(repeating: "a", count: 63))
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("token")))
    }

    @Test("non-hex token returns .invalidField(token)")
    func nonHexToken() throws {
        let p = validPayload(token: String(repeating: "z", count: 64))
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("token")))
    }

    @Test("non-hex fp returns .invalidField(fp)")
    func nonHexFP() throws {
        let p = validPayload(fp: String(repeating: "z", count: 64))
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("fp")))
    }

    @Test("port 0 returns .invalidField(port)")
    func portZero() throws {
        let p = validPayload(port: 0)
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("port")))
    }

    @Test("port 70000 returns .invalidField(port)")
    func portTooHigh() throws {
        let p = validPayload(port: 70000)
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("port")))
    }

    @Test("empty host returns .invalidField(host)")
    func emptyHost() throws {
        let p = validPayload(host: "")
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.invalidField("host")))
    }

    @Test("missing field (no fp key) returns .malformedJSON")
    func missingField() {
        // Omit the "fp" key to simulate a missing required field
        let json = #"{"v":1,"host":"100.1.2.3","port":8443,"token":"\#(validToken)"}"#
        let result = PairingPayload.decode(fromQRString: json)
        #expect(result == .failure(.malformedJSON))
    }

    @Test("uppercase hex token and fp are accepted")
    func uppercaseHex() throws {
        let p = validPayload(
            token: String(repeating: "A", count: 64),
            fp:    String(repeating: "B", count: 64)
        )
        let json = try #require(p.jsonString())
        let result = PairingPayload.decode(fromQRString: json)
        switch result {
        case .success(let decoded): #expect(decoded == p)
        case .failure(let err): Issue.record("Unexpected failure: \(err)")
        }
    }
}

// MARK: - QR image round-trip (proves makeQRImage → decodeQR → decode path)

@Suite("PairingQR image round-trip")
struct PairingQRTests {

    @Test("makeQRImage produces an image")
    func makeImage() {
        let p = validPayload()
        let img = PairingQR.makeQRImage(p)
        #expect(img != nil)
    }

    @Test("makeQRImage → decodeQR returns a decodable payload string")
    func decodeQRString() throws {
        let p     = validPayload()
        let img   = try #require(PairingQR.makeQRImage(p))
        let raw   = try #require(PairingQR.decodeQR(from: img))
        // JSON key order is not guaranteed by JSONEncoder, so compare decoded struct, not string.
        let result = PairingPayload.decode(fromQRString: raw)
        switch result {
        case .success(let decoded): #expect(decoded == p)
        case .failure(let err): Issue.record("QR string was not decodable: \(err)")
        }
    }

    /// Full path: encode → QR image → QR decode → payload decode == original.
    /// This is the exact sequence used on device (minus the AVCaptureSession camera step,
    /// which is TAG:UNTESTED).
    @Test("full round-trip: payload → QR image → decoded payload equals original")
    func fullRoundTrip() throws {
        let original = validPayload()
        let img      = try #require(PairingQR.makeQRImage(original))
        let raw      = try #require(PairingQR.decodeQR(from: img))
        let result   = PairingPayload.decode(fromQRString: raw)
        switch result {
        case .success(let decoded): #expect(decoded == original)
        case .failure(let err):    Issue.record("QR round-trip failed: \(err)")
        }
    }
}

// MARK: - CockpitConfig.apply

@MainActor
@Suite("CockpitConfig.apply")
struct CockpitConfigApplyTests {

    @Test("apply sets all four fields")
    func applySetsFields() {
        let suite = makeSuite()
        let cfg = CockpitConfig(defaults: suite)
        let p = validPayload()
        cfg.apply(p)
        #expect(cfg.host == p.host)
        #expect(cfg.portString == String(p.port))
        #expect(cfg.pairingToken == p.token)
        #expect(cfg.pinnedFingerprint == p.fp)
    }

    @Test("apply persists to UserDefaults (survives re-init from same suite)")
    func applyPersists() {
        let suite = makeSuite()
        let cfg1 = CockpitConfig(defaults: suite)
        let p = validPayload(host: "192.168.1.50", port: 9443)
        cfg1.apply(p)

        let cfg2 = CockpitConfig(defaults: suite)
        #expect(cfg2.host == "192.168.1.50")
        #expect(cfg2.portString == "9443")
        #expect(cfg2.pairingToken == p.token)
        #expect(cfg2.pinnedFingerprint == p.fp)
    }

    @Test("apply overwrites previous config")
    func applyOverwrites() {
        let suite = makeSuite()
        let cfg = CockpitConfig(defaults: suite)
        cfg.apply(validPayload(host: "old.host", port: 8443))
        cfg.apply(validPayload(host: "new.host", port: 9000))
        #expect(cfg.host == "new.host")
        #expect(cfg.portString == "9000")
    }
}
