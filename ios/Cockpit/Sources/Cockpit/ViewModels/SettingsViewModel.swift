// CV5 — SettingsViewModel: validates + commits ptyd TLS connection config.
// CV6-A — adds QR-scan sheet state + apply(payload) path.
// Fields: host, port, pairingToken, pinnedFingerprint (replaces ws URL + dev token).
import Foundation
import Combine

@MainActor
public final class SettingsViewModel: ObservableObject {

    // MARK: - Validation errors

    public enum ValidationError: Equatable {
        case emptyHost
        case emptyPort
        case invalidPort        // not a valid UInt16
        case emptyToken
        case emptyFingerprint
        case invalidFingerprint // not a 64-char hex string
    }

    // MARK: - State

    @Published public var hostDraft: String
    @Published public var portDraft: String
    @Published public var tokenDraft: String
    @Published public var fingerprintDraft: String

    @Published public private(set) var validationErrors: [ValidationError] = []

    // CV6-A — QR scanner sheet
    @Published public var showQRScanner = false
    @Published public var qrDecodeError: PairingError?

    private let config: CockpitConfig

    // MARK: - Init

    public init(config: CockpitConfig) {
        self.config = config
        self.hostDraft        = config.host
        self.portDraft        = config.portString
        self.tokenDraft       = config.pairingToken
        self.fingerprintDraft = config.pinnedFingerprint
    }

    // MARK: - Validation

    @discardableResult
    public func validate() -> Bool {
        var errors: [ValidationError] = []

        let trimHost = hostDraft.trimmingCharacters(in: .whitespaces)
        if trimHost.isEmpty { errors.append(.emptyHost) }

        let trimPort = portDraft.trimmingCharacters(in: .whitespaces)
        if trimPort.isEmpty {
            errors.append(.emptyPort)
        } else if UInt16(trimPort) == nil {
            errors.append(.invalidPort)
        }

        let trimToken = tokenDraft.trimmingCharacters(in: .whitespaces)
        if trimToken.isEmpty { errors.append(.emptyToken) }

        let trimFP = fingerprintDraft.trimmingCharacters(in: .whitespaces)
        if trimFP.isEmpty {
            errors.append(.emptyFingerprint)
        } else if !isValidSHA256Hex(trimFP) {
            errors.append(.invalidFingerprint)
        }

        validationErrors = errors
        return errors.isEmpty
    }

    // MARK: - Save

    @discardableResult
    public func save() -> Bool {
        guard validate() else { return false }
        config.host              = hostDraft.trimmingCharacters(in: .whitespaces)
        config.portString        = portDraft.trimmingCharacters(in: .whitespaces)
        config.pairingToken      = tokenDraft.trimmingCharacters(in: .whitespaces)
        config.pinnedFingerprint = fingerprintDraft.trimmingCharacters(in: .whitespaces)
        return true
    }

    // MARK: - CV6-A: QR apply

    /// Called with the raw string from `QRScannerView.onScan`.
    /// On success: applies payload to config and dismisses the sheet (sets `showQRScanner = false`).
    /// On failure: sets `qrDecodeError`; caller can display inline.
    public func handleScannedQR(_ raw: String) {
        qrDecodeError = nil
        switch PairingPayload.decode(fromQRString: raw) {
        case .success(let payload):
            config.apply(payload)
            // Sync drafts so the manual fields reflect the new values if the user re-opens settings.
            hostDraft        = config.host
            portDraft        = config.portString
            tokenDraft       = config.pairingToken
            fingerprintDraft = config.pinnedFingerprint
            showQRScanner    = false
        case .failure(let err):
            qrDecodeError = err
        }
    }

    // MARK: - Reset

    public func reset() {
        hostDraft        = CockpitConfig.defaultHost
        portDraft        = String(CockpitConfig.defaultPort)
        tokenDraft       = ""
        fingerprintDraft = ""
        validationErrors = []
        qrDecodeError    = nil
    }

    // MARK: - Helpers

    /// SHA-256 fingerprint: exactly 64 lowercase or uppercase hex characters.
    private func isValidSHA256Hex(_ s: String) -> Bool {
        let trimmed = s.trimmingCharacters(in: .whitespaces)
        guard trimmed.count == 64 else { return false }
        return trimmed.allSatisfy { $0.isHexDigit }
    }
}
