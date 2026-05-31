// CV6-B — iCloud Keychain best-effort auto-fill (iOS side).
//
// Design invariants (from CV6-PAIRING-SPEC.md §3/§5/§6):
//   • iOS only READS — the Mac app publishes the keychain item (separate work).
//   • Never blocks: if item absent or undecodable → fall through to QR/manual UI.
//   • "Paired" only after successful hello round-trip (connectionState == .connected).
//     A keychain read alone does NOT reach .paired.
//   • kSecUseDataProtectionKeychain = true on every query (Square Valet lesson).
//   • Explicit shared access group on every query.
//
// TAG:UNTESTED — real synchronizable keychain read + cross-device sync requires
//   entitlement-signed build + two same-Apple-ID devices. All logic is fully
//   covered by StubKeychainReader in unit tests.

import Foundation
import Security

// MARK: - Access-group constant

/// Keychain access group shared between the iOS app and the Mac app.
/// The entitlement carries `$(AppIdentifierPrefix)ai.rosslabs.cockpit.pairing`;
/// Security.framework substitutes the real Team ID prefix at runtime, so we
/// supply the bare group string here and let the OS resolve it.
public let kCockpitPairingAccessGroup = "ai.rosslabs.cockpit.pairing"

/// kSecAttrService value for the pairing item.
public let kCockpitPairingService = "ai.rosslabs.cockpit.pairing"

// MARK: - KeychainReading protocol

/// Abstraction over SecItemCopyMatching so logic is unit-testable without a real keychain.
public protocol KeychainReading: Sendable {
    func readPairingItem() -> Data?
}

// MARK: - SyncedKeychainReader (production)

/// Reads the iCloud Keychain pairing item written by the Mac app.
/// Returns the raw JSON `Data` for the pairing payload, or nil if absent/inaccessible.
public struct SyncedKeychainReader: KeychainReading {
    public init() {}

    public func readPairingItem() -> Data? {
        let query: [CFString: Any] = [
            kSecClass:                    kSecClassGenericPassword,
            kSecAttrService:              kCockpitPairingService,
            kSecAttrSynchronizable:       kSecAttrSynchronizableAny,
            kSecUseDataProtectionKeychain: true as CFBoolean,
            kSecAttrAccessGroup:          kCockpitPairingAccessGroup,
            kSecReturnData:               true as CFBoolean,
            kSecMatchLimit:               kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess, let data = item as? Data else { return nil }
        return data
    }
}

// MARK: - PairingSource

/// Where a pairing payload arrived from — used for logging/analytics and state labelling.
public enum PairingSource: String, Equatable, Sendable {
    case keychain
    case qr
    case manual
}

// MARK: - PairingCoordinatorState

/// State machine for the iCloud Keychain auto-fill path (Tier B).
///
/// Transitions:
///   unpaired
///     → attempting(source:)   — read in progress
///     → verifying             — payload applied, connect() called; awaiting hello_ack
///     → paired                — connectionState became .connected (hello_ack OK)
///     → needsManual(reason:)  — read failed, decode failed, or connect rejected
///
/// IMPORTANT: `.paired` is only reached via a real `.connected` event from
/// `SessionStore.connectionState` — a keychain read alone never moves to `.paired`.
public enum PairingCoordinatorState: Equatable, Sendable {
    case unpaired
    case attempting(source: PairingSource)
    case verifying
    case paired
    case needsManual(reason: String)

    // MARK: Equatable
    public static func == (lhs: PairingCoordinatorState, rhs: PairingCoordinatorState) -> Bool {
        switch (lhs, rhs) {
        case (.unpaired, .unpaired):           return true
        case (.verifying, .verifying):         return true
        case (.paired, .paired):               return true
        case (.attempting(let a), .attempting(let b)): return a == b
        case (.needsManual(let a), .needsManual(let b)): return a == b
        default:                               return false
        }
    }

    // MARK: - UI helpers (text-color status surface — no badge per Calm Precision)

    /// Short user-facing label. Views apply colour via `statusColor`; no background badge.
    public var statusLabel: String {
        switch self {
        case .unpaired:             return "Waiting for pairing"
        case .attempting:           return "Checking iCloud Keychain…"
        case .verifying:            return "Verifying connection…"
        case .paired:               return "Paired"
        case .needsManual(let r):   return r
        }
    }

    /// True when the UI should surface the keychain-pairing status row.
    public var isSurfaceable: Bool {
        switch self {
        case .unpaired, .paired: return false
        default:                 return true
        }
    }
}

// MARK: - attemptKeychainPairing

/// Attempt to auto-fill from the iCloud Keychain item. Best-effort: never blocks.
///
/// - Parameters:
///   - reader:          A `KeychainReading` (real or stub in tests).
///   - applyAndConnect: Called on the main actor when a valid payload is found.
///                      Responsible for `config.apply(payload)` + `store.connect()`.
/// - Returns: `true` if a valid payload was read and `applyAndConnect` was called;
///            `false` if absent or undecodable (caller should show QR/manual UI).
///
/// Thread-safety: may be called from any context; `applyAndConnect` is dispatched to
/// `@MainActor` by the caller (see `PairingCoordinator.attemptKeychain()`).
@discardableResult
public func attemptKeychainPairing(
    reader: KeychainReading,
    applyAndConnect: (PairingPayload) -> Void
) -> Bool {
    guard let data = reader.readPairingItem() else { return false }
    guard let json = String(data: data, encoding: .utf8) else { return false }
    // Reuse CV6-A's decode+validate path — same payload schema.
    switch PairingPayload.decode(fromQRString: json) {
    case .success(let payload):
        applyAndConnect(payload)
        return true
    case .failure:
        return false
    }
}

// MARK: - needsManualMessage

/// Returns the standard user-facing message when keychain auto-fill is unavailable.
/// Kept as a pure function so it's injectable in tests without a real coordinator.
public func needsManualMessage() -> String {
    "Couldn't auto-pair — scan the QR from Easy Terminal, or check iCloud Keychain is on."
}

// MARK: - PairingCoordinator

/// Coordinates the Tier-B keychain auto-fill path for a `SessionStore`.
///
/// Usage (on the main actor):
/// ```swift
/// let coordinator = PairingCoordinator(store: store, reader: SyncedKeychainReader())
/// coordinator.attemptKeychain()   // call on launch when config has no token
/// // bind coordinator.state to a View for status display
/// ```
@MainActor
public final class PairingCoordinator: ObservableObject {
    @Published public private(set) var state: PairingCoordinatorState = .unpaired

    private let store: SessionStore
    private let reader: KeychainReading
    private var connectionObserver: Task<Void, Never>?

    public init(store: SessionStore, reader: KeychainReading = SyncedKeychainReader()) {
        self.store  = store
        self.reader = reader
    }

    deinit {
        connectionObserver?.cancel()
    }

    // MARK: - Public API

    /// Best-effort keychain auto-fill. Call when unpaired (no token in config).
    ///
    /// Flow:
    ///   1. Move to `.attempting(.keychain)`.
    ///   2. Try `attemptKeychainPairing`.
    ///   3a. Success → `config.apply` + `store.connect()` + move to `.verifying`.
    ///       Watch `store.connectionState`; when `.connected` → `.paired`;
    ///       when `.error` → `.needsManual`.
    ///   3b. Failure → `.needsManual` immediately; QR/manual UI remains available.
    ///
    /// This method never blocks and never waits for iCloud sync — if the item is
    /// absent the caller falls through to the existing QR/manual Settings UI.
    public func attemptKeychain() {
        state = .attempting(source: .keychain)

        let found = attemptKeychainPairing(reader: reader) { [weak self] payload in
            guard let self else { return }
            self.store.config.apply(payload)
            self.store.connect()
        }

        if found {
            state = .verifying
            startObservingConnection()
        } else {
            state = .needsManual(reason: needsManualMessage())
        }
    }

    /// Called by the QR/manual path after pairing fields are filled; moves to `.verifying`.
    public func didApplyManualOrQRPairing(source: PairingSource) {
        state = .attempting(source: source)
        store.connect()
        state = .verifying
        startObservingConnection()
    }

    /// Reset to `.unpaired` (e.g. after explicit "Re-pair" action).
    public func reset() {
        connectionObserver?.cancel()
        connectionObserver = nil
        state = .unpaired
    }

    // MARK: - Private

    /// Watch `store.connectionState` and promote/demote coordinator state accordingly.
    /// Runs until `.paired` or `.needsManual` — then cancels itself.
    private func startObservingConnection() {
        connectionObserver?.cancel()
        connectionObserver = Task { @MainActor [weak self] in
            guard let self else { return }
            // Poll via async stream on the published property.
            for await connState in self.store.$connectionState.values {
                guard let self = self as PairingCoordinator? else { return }
                switch connState {
                case .connected:
                    // hello_ack received — this is the only path to .paired.
                    self.state = .paired
                    self.connectionObserver?.cancel()
                    self.connectionObserver = nil
                    return
                case .error(let msg):
                    self.state = .needsManual(reason: msg)
                    self.connectionObserver?.cancel()
                    self.connectionObserver = nil
                    return
                case .idle, .connecting, .disconnected:
                    break   // keep waiting
                }
            }
        }
    }
}
