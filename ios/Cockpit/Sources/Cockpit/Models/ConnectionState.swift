// G4 — Store-level connection lifecycle state (distinct from CockpitClient.ConnectionState).
// This enum is what views bind to; CockpitClient.ConnectionState drives it via SessionStore.

import Foundation

/// Lifecycle state exposed by SessionStore to the UI.
public enum ConnectionState: Equatable, Sendable {
    /// No connection attempt has been made (app just launched or config cleared).
    case idle
    /// Handshake in progress — socket open, waiting for hello_ok.
    case connecting
    /// hello_ok received; wire is live.
    case connected
    /// Socket closed after a successful connection.
    case disconnected(reason: String?)
    /// Terminal failure: bad config, socket error, auth rejection.
    case error(message: String)

    // MARK: - Equatable

    public static func == (lhs: ConnectionState, rhs: ConnectionState) -> Bool {
        switch (lhs, rhs) {
        case (.idle, .idle): return true
        case (.connecting, .connecting): return true
        case (.connected, .connected): return true
        case (.disconnected(let a), .disconnected(let b)): return a == b
        case (.error(let a), .error(let b)): return a == b
        default: return false
        }
    }

    // MARK: - Helpers

    /// User-facing short label for the status indicator.
    public var label: String {
        switch self {
        case .idle:           return "Not connected"
        case .connecting:     return "Connecting…"
        case .connected:      return "Connected"
        case .disconnected:   return "Disconnected"
        case .error:          return "Connection error"
        }
    }

    /// Inline diagnostic message shown in the error banner; nil when nothing to show.
    public var bannerMessage: String? {
        switch self {
        case .error(let msg):              return msg
        case .disconnected(let r):         return r
        default:                           return nil
        }
    }

    /// True when the state warrants showing the error banner with a Settings link.
    public var needsBanner: Bool { bannerMessage != nil }
}
