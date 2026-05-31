// G3 — SettingsViewModel: validates + commits connection config.
import Foundation
import Combine

@MainActor
public final class SettingsViewModel: ObservableObject {

    // MARK: - Validation errors

    public enum ValidationError: Equatable {
        case emptyURL
        case invalidURLScheme   // must be ws:// or wss://
        case malformedURL
        case emptyToken
    }

    // MARK: - State

    @Published public var urlDraft: String
    @Published public var tokenDraft: String

    /// Non-nil when the current drafts have a validation problem.
    @Published public private(set) var validationErrors: [ValidationError] = []

    private let config: CockpitConfig

    // MARK: - Init

    public init(config: CockpitConfig) {
        self.config = config
        self.urlDraft   = config.daemonURLString
        self.tokenDraft = config.devToken
    }

    // MARK: - Validation

    /// Validates the current drafts and updates `validationErrors`.
    /// Returns true if valid.
    @discardableResult
    public func validate() -> Bool {
        var errors: [ValidationError] = []

        let trimmedURL = urlDraft.trimmingCharacters(in: .whitespaces)
        if trimmedURL.isEmpty {
            errors.append(.emptyURL)
        } else if let url = URL(string: trimmedURL), let scheme = url.scheme {
            if scheme != "ws" && scheme != "wss" {
                errors.append(.invalidURLScheme)
            }
        } else {
            errors.append(.malformedURL)
        }

        let trimmedToken = tokenDraft.trimmingCharacters(in: .whitespaces)
        if trimmedToken.isEmpty {
            errors.append(.emptyToken)
        }

        validationErrors = errors
        return errors.isEmpty
    }

    // MARK: - Save

    /// Validates and, if valid, persists drafts to the config.
    /// Returns true if saved.
    @discardableResult
    public func save() -> Bool {
        guard validate() else { return false }
        config.daemonURLString = urlDraft.trimmingCharacters(in: .whitespaces)
        config.devToken        = tokenDraft.trimmingCharacters(in: .whitespaces)
        return true
    }

    // MARK: - Reset

    public func reset() {
        urlDraft   = CockpitConfig.defaultDaemonURL
        tokenDraft = ""
        validationErrors = []
    }
}
