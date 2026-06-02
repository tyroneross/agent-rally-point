// CV6-A — QR pairing payload schema + validation.
// Shared by the QR scan path and (later) the iCloud Keychain path.

import Foundation

// MARK: - Error

/// Typed errors returned by `PairingPayload.decode(fromQRString:)`.
/// Never crash — all bad input produces a typed failure.
public enum PairingError: Error, Equatable {
    case malformedJSON
    case badVersion(Int)
    case invalidField(String)   // field name, e.g. "host", "port", "token", "fp"
}

// MARK: - Payload

/// One JSON object carried in the pairing QR (and later the iCloud Keychain blob).
/// ```json
/// { "v": 1, "host": "100.x.y.z", "port": 8443,
///   "token": "<64-hex>", "fp": "<64-hex>" }
/// ```
public struct PairingPayload: Codable, Equatable {

    public let v:     Int
    public let host:  String
    public let port:  Int
    public let token: String
    public let fp:    String

    public init(v: Int, host: String, port: Int, token: String, fp: String) {
        self.v     = v
        self.host  = host
        self.port  = port
        self.token = token
        self.fp    = fp
    }

    // CodingKeys are the default (field names match JSON keys exactly).

    // MARK: - Encode

    /// Serialise to a compact JSON string. Returns nil only if `JSONEncoder` fails (should never happen).
    public func jsonString() -> String? {
        guard let data = try? JSONEncoder().encode(self) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    // MARK: - Decode + validate

    private static let hexPattern = try! NSRegularExpression(pattern: "^[0-9a-fA-F]{64}$")

    private static func isHex64(_ s: String) -> Bool {
        let range = NSRange(s.startIndex..., in: s)
        return hexPattern.firstMatch(in: s, range: range) != nil
    }

    /// Decode **and validate** a QR string → `PairingPayload`.
    /// Returns `.failure` with a typed `PairingError` on any problem; never throws or crashes.
    public static func decode(fromQRString raw: String) -> Result<PairingPayload, PairingError> {
        guard let data = raw.data(using: .utf8),
              let p = try? JSONDecoder().decode(PairingPayload.self, from: data)
        else {
            return .failure(.malformedJSON)
        }

        // Version gate
        guard p.v == 1 else { return .failure(.badVersion(p.v)) }

        // host non-empty
        guard !p.host.trimmingCharacters(in: .whitespaces).isEmpty else {
            return .failure(.invalidField("host"))
        }

        // port 1…65535
        guard (1...65535).contains(p.port) else {
            return .failure(.invalidField("port"))
        }

        // token: exactly 64 hex chars
        guard isHex64(p.token) else {
            return .failure(.invalidField("token"))
        }

        // fp: exactly 64 hex chars
        guard isHex64(p.fp) else {
            return .failure(.invalidField("fp"))
        }

        return .success(p)
    }
}
