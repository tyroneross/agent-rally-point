// G3 — Persisted connection config (daemon URL + dev token).
// Stored in UserDefaults; phone-only, no sync.
// TAG:UNTESTED on device — ws:// works in sim, wss:// path not exercised.

import Foundation

/// Persisted connection settings for the Cockpit daemon.
/// Stored in UserDefaults under keys prefixed with `cockpit.config.`.
public final class CockpitConfig: ObservableObject {

    // MARK: - Keys

    private enum Key {
        static let daemonURL = "cockpit.config.daemonURL"
        static let devToken  = "cockpit.config.devToken"
    }

    // MARK: - Defaults

    public static let defaultDaemonURL = "ws://127.0.0.1:8787"

    // MARK: - Storage

    private let defaults: UserDefaults

    // MARK: - Published properties

    @Published public var daemonURLString: String {
        didSet { defaults.set(daemonURLString, forKey: Key.daemonURL) }
    }

    @Published public var devToken: String {
        didSet { defaults.set(devToken, forKey: Key.devToken) }
    }

    // MARK: - Init

    /// `defaults` can be overridden in tests to avoid polluting the app suite.
    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        self.daemonURLString = defaults.string(forKey: Key.daemonURL) ?? CockpitConfig.defaultDaemonURL
        self.devToken        = defaults.string(forKey: Key.devToken)  ?? ""
    }

    // MARK: - Derived

    /// Parsed and validated daemon URL, or nil if the stored string is invalid.
    public var daemonURL: URL? {
        guard let url = URL(string: daemonURLString),
              let scheme = url.scheme,
              (scheme == "ws" || scheme == "wss"),
              url.host != nil else { return nil }
        return url
    }

    /// True when the config is sufficient to attempt a connection.
    public var isConnectable: Bool {
        daemonURL != nil && !devToken.trimmingCharacters(in: .whitespaces).isEmpty
    }
}
