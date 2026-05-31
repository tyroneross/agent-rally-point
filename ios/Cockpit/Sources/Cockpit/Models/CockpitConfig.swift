// CV5 — Persisted connection config for ptyd TLS thin client.
// Replaces ws URL + dev token with: host, port, pairingToken, pinnedFingerprint.
// Stored in UserDefaults; phone-only, no sync.
// TAG:UNTESTED on device — live TLS connection requires a running daemon + tunnel.

import Foundation

/// Persisted connection settings for the ptyd daemon.
/// Stored in UserDefaults under keys prefixed with `cockpit.config.`.
public final class CockpitConfig: ObservableObject {

    // MARK: - Keys

    private enum Key {
        static let host               = "cockpit.config.host"
        static let port               = "cockpit.config.port"
        static let pairingToken       = "cockpit.config.pairingToken"
        static let pinnedFingerprint  = "cockpit.config.pinnedFingerprint"
    }

    // MARK: - Defaults

    /// loopback — reached via user's tunnel (Tailscale/SSH forward).
    public static let defaultHost = "127.0.0.1"
    public static let defaultPort: UInt16 = 7333

    // MARK: - Storage

    private let defaults: UserDefaults

    // MARK: - Published properties

    @Published public var host: String {
        didSet { defaults.set(host, forKey: Key.host) }
    }

    @Published public var portString: String {
        didSet { defaults.set(portString, forKey: Key.port) }
    }

    /// 64-character hex pairing token stored at ~/.config/ptyd/pairing_token on the daemon.
    @Published public var pairingToken: String {
        didSet { defaults.set(pairingToken, forKey: Key.pairingToken) }
    }

    /// SHA-256 fingerprint (hex) of the daemon's self-signed TLS cert.
    /// Printed to stderr on daemon startup and in `ptyd status server --json` as `tls_fingerprint`.
    @Published public var pinnedFingerprint: String {
        didSet { defaults.set(pinnedFingerprint, forKey: Key.pinnedFingerprint) }
    }

    // MARK: - Init

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.host              = defaults.string(forKey: Key.host) ?? CockpitConfig.defaultHost
        self.portString        = defaults.string(forKey: Key.port) ?? String(CockpitConfig.defaultPort)
        self.pairingToken      = defaults.string(forKey: Key.pairingToken)      ?? ""
        self.pinnedFingerprint = defaults.string(forKey: Key.pinnedFingerprint) ?? ""
    }

    // MARK: - Derived

    /// Parsed port, or nil if portString is not a valid UInt16.
    public var port: UInt16? {
        guard let n = UInt16(portString.trimmingCharacters(in: .whitespaces)) else { return nil }
        return n
    }

    /// True when all four fields are present and port is parseable.
    public var isConnectable: Bool {
        let h = host.trimmingCharacters(in: .whitespaces)
        let t = pairingToken.trimmingCharacters(in: .whitespaces)
        let f = pinnedFingerprint.trimmingCharacters(in: .whitespaces)
        return !h.isEmpty && port != nil && !t.isEmpty && !f.isEmpty
    }
}
