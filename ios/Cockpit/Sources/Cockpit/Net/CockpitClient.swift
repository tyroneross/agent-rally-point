// D1 — WebSocket client using URLSessionWebSocketTask
// TAG:UNTESTED — dev bearer token auth; SE-mTLS replaces this in production.
// TAG:UNTESTED — Full round-trip test requires a running cockpitd daemon.
// G3 — URL + token now injected at call site (from CockpitConfig persisted store).

import Foundation

// MARK: - Client

@MainActor
public final class CockpitClient: NSObject, ObservableObject {

    public enum ConnectionState: Equatable, Sendable {
        case disconnected
        case connecting
        case authenticated
        case failed(String)
    }

    @Published public private(set) var connectionState: ConnectionState = .disconnected

    /// Incoming server frames for subscribers.
    public var onFrame: ((ServerFrame) -> Void)?

    private var task: URLSessionWebSocketTask?
    private var session: URLSession?
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    public override init() {
        super.init()
        encoder.outputFormatting = .sortedKeys
    }

    // MARK: - Connect

    public func connect(to url: URL, token: String) {
        guard connectionState == .disconnected else { return }
        connectionState = .connecting

        let cfg = URLSessionConfiguration.default
        let urlSession = URLSession(configuration: cfg, delegate: nil, delegateQueue: .main)
        session = urlSession
        let wsTask = urlSession.webSocketTask(with: url)
        task = wsTask
        wsTask.resume()

        // First frame MUST be hello (wire contract §Auth)
        Task { [weak self] in
            guard let self else { return }
            do {
                try await self.sendCommand(.hello(token: token))
                self.scheduleReceive()
            } catch {
                self.connectionState = .failed(error.localizedDescription)
            }
        }
    }

    public func disconnect() {
        task?.cancel(with: .normalClosure, reason: nil)
        task = nil
        session = nil
        connectionState = .disconnected
    }

    // MARK: - Send

    public func sendCommand(_ cmd: ClientCommand) async throws {
        guard let task else { throw CockpitError.notConnected }
        let data = try encoder.encode(cmd)
        guard let text = String(data: data, encoding: .utf8) else {
            throw CockpitError.encodingFailed
        }
        try await task.send(.string(text))
    }

    // Convenience shortcuts used by view-models

    public func openSession(id: String, fromSeq: UInt64) async throws {
        try await sendCommand(.openSession(sessionId: id, fromSeq: fromSeq))
    }

    public func sendPrompt(sessionId: String, text: String) async throws {
        try await sendCommand(.sendPrompt(sessionId: sessionId, text: text))
    }

    public func steer(sessionId: String, text: String) async throws {
        try await sendCommand(.steer(sessionId: sessionId, text: text))
    }

    public func approve(approvalId: String, decision: ApprovalDecision, reason: String? = nil) async throws {
        try await sendCommand(.approve(approvalId: approvalId, decision: decision, reason: reason))
    }

    public func launchSession(agentType: String, repoPath: String, prompt: String? = nil) async throws {
        try await sendCommand(.launchSession(agentType: agentType, repoPath: repoPath, prompt: prompt))
    }

    // MARK: - Receive loop

    private func scheduleReceive() {
        task?.receive { [weak self] result in
            guard let self else { return }
            Task { @MainActor in
                switch result {
                case .success(let msg):
                    self.handle(message: msg)
                    self.scheduleReceive()
                case .failure(let err):
                    self.connectionState = .failed(err.localizedDescription)
                }
            }
        }
    }

    private func handle(message: URLSessionWebSocketTask.Message) {
        let data: Data
        switch message {
        case .string(let text):
            guard let d = text.data(using: .utf8) else { return }
            data = d
        case .data(let d):
            data = d
        @unknown default:
            return
        }

        do {
            let frame = try decoder.decode(ServerFrame.self, from: data)
            if case .helloOk = frame {
                connectionState = .authenticated
            }
            onFrame?(frame)
        } catch {
            // Protocol error — log and keep running
            print("[CockpitClient] decode error: \(error)")
        }
    }
}

// MARK: - Errors

public enum CockpitError: Error, LocalizedError {
    case notConnected
    case encodingFailed

    public var errorDescription: String? {
        switch self {
        case .notConnected:   return "WebSocket not connected."
        case .encodingFailed: return "Failed to encode command."
        }
    }
}
