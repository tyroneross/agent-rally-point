// CV5 — ptyd TLS thin client.
// Replaces URLSessionWebSocketTask with Network.framework NWConnection + NWProtocolTLS.
// Cert pinning: custom sec_protocol_options_set_verify_block — only accepts the server
// cert whose SHA-256 fingerprint matches config.pinnedFingerprint. No CA trust.
// Auth-before-access: first framed request is the ptyd `hello` handshake (mobile.rs).
// Protocol: newline-delimited JSON-RPC — `{"id","method","params"}\n` → `{"id","result":...}\n`.
//
// TAG:UNTESTED — live TLS connection requires a running ptyd daemon + SSH/Tailscale tunnel
// and a real device or simulator with the tunnel reachable. Unit tests cover JSON framing,
// hello encoding, Event decoding, and cert fingerprint comparison without a live socket.

import Foundation
import Network
import CryptoKit
import Security

// MARK: - CockpitClient

@MainActor
public final class CockpitClient: ObservableObject {

    @Published public private(set) var connectionState: ConnectionState = .idle

    /// Called on every complete JSON line received from the daemon (after the hello ack).
    public var onLine: ((Data) -> Void)?

    // Backing NWConnection — nil when disconnected.
    private var connection: NWConnection?

    // Accumulation buffer for the line-frame reader.
    private var readBuffer = Data()

    // Request-ID counter.
    private var nextID: Int = 1

    private let encoder = JSONEncoder()

    public init() {
        encoder.outputFormatting = .sortedKeys
    }

    // MARK: - Connect

    /// Establish a TLS connection to `host:port`, pin the cert to `pinnedFingerprint`,
    /// then perform the ptyd hello handshake with `pairingToken`.
    public func connect(host: String, port: UInt16, pairingToken: String, pinnedFingerprint: String) {
        guard connectionState == .idle || {
            if case .disconnected = connectionState { return true }
            return false
        }() else { return }

        connectionState = .connecting

        let tlsOptions = NWProtocolTLS.Options()

        // Cert pinning via sec_protocol_options_set_verify_block.
        // The verify block receives the sec_protocol_metadata — we walk the peer
        // certificate chain, take the leaf (first certificate), compute its SHA-256
        // over the DER bytes, and compare to the configured pinned fingerprint.
        // NO CA trust is used — we only accept a cert whose fingerprint matches exactly.
        let pinned = pinnedFingerprint
        sec_protocol_options_set_verify_block(
            tlsOptions.securityProtocolOptions,
            { metadata, _, complete in
                // Collect the peer's certificate chain from the TLS metadata.
                var leafDER: Data? = nil
                sec_protocol_metadata_access_peer_certificate_chain(metadata) { secCert in
                    // Only care about the first (leaf) cert.
                    guard leafDER == nil else { return }
                    let certRef: SecCertificate = sec_certificate_copy_ref(secCert).takeRetainedValue()
                    leafDER = SecCertificateCopyData(certRef) as Data
                }
                guard let der = leafDER else {
                    complete(false)
                    return
                }
                let digest = SHA256.hash(data: der)
                let hex = digest.map { String(format: "%02x", $0) }.joined()
                complete(CockpitClient.fingerprintsMatch(hex, pinned))
            },
            .global()
        )

        let params = NWParameters(tls: tlsOptions, tcp: NWProtocolTCP.Options())
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            connectionState = .error(message: "Invalid port: \(port)")
            return
        }
        let conn = NWConnection(
            host: NWEndpoint.Host(host),
            port: nwPort,
            using: params
        )
        connection = conn

        conn.stateUpdateHandler = { [weak self] state in
            Task { @MainActor [weak self] in
                self?.handleStateUpdate(state, pairingToken: pairingToken)
            }
        }
        conn.start(queue: .global(qos: .userInitiated))
    }

    public func disconnect() {
        connection?.cancel()
        connection = nil
        readBuffer = Data()
        connectionState = .disconnected(reason: nil)
    }

    // MARK: - NWConnection state handler

    private func handleStateUpdate(_ state: NWConnection.State, pairingToken: String) {
        switch state {
        case .ready:
            // TLS handshake succeeded (cert pinning passed). Send hello.
            sendHello(pairingToken: pairingToken)
            scheduleRead()
        case .failed(let err):
            let msg = err.localizedDescription
            connectionState = .error(message: "TLS connection failed: \(msg)")
            connection = nil
        case .cancelled:
            if case .connected = connectionState {
                connectionState = .disconnected(reason: nil)
            }
        case .waiting(let err):
            connectionState = .error(message: "Waiting: \(err.localizedDescription)")
        default:
            break
        }
    }

    // MARK: - Hello handshake

    private func sendHello(pairingToken: String) {
        let id = allocID()
        let request = PtydRequest(
            id: id,
            method: "hello",
            params: HelloParams(protocolVersion: 1, token: pairingToken)
        )
        guard let data = try? encoder.encode(request),
              var line = String(data: data, encoding: .utf8) else {
            connectionState = .error(message: "Failed to encode hello")
            return
        }
        line += "\n"
        writeLine(line)
        // The hello ack is handled in the normal line-read path (handleLine).
    }

    // MARK: - Request sending

    /// Send a raw ptyd JSON-RPC request. Returns the request ID.
    @discardableResult
    public func sendRequest(method: String, params: some Encodable) -> String {
        let id = allocID()
        let wrapper = PtydRequestDynamic(id: id, method: method)
        // Encode params into a JSON object, then merge with wrapper manually.
        guard let paramsData = try? encoder.encode(params),
              var paramsObj = (try? JSONSerialization.jsonObject(with: paramsData)) as? [String: Any] else {
            return id
        }
        var obj: [String: Any] = ["id": id, "method": method, "params": paramsObj]
        // Suppress unused warning — obj is written below.
        _ = paramsObj
        guard let lineData = try? JSONSerialization.data(withJSONObject: obj),
              let lineStr = String(data: lineData, encoding: .utf8) else {
            return id
        }
        writeLine(lineStr + "\n")
        return id
    }

    // MARK: - High-level ptyd methods

    /// Subscribe to structured agent events.
    /// Returns the request ID. Caller observes `onLine` for ack then Event stream.
    @discardableResult
    public func subscribeStructured(
        sessionID: String,
        agent: String,
        baseDir: String,
        from: Int? = nil
    ) -> String {
        var params: [String: Any] = [
            "session_id": sessionID,
            "agent": agent,
            "base_dir": baseDir
        ]
        if let f = from { params["from"] = f }
        return sendRaw(method: "agent.subscribe_structured", params: params)
    }

    /// Retrieve audit log entries. Pass nil sessionID for all sessions.
    @discardableResult
    public func getAudit(sessionID: String? = nil, limit: Int? = nil) -> String {
        var params: [String: Any] = [:]
        if let sid = sessionID { params["session_id"] = sid }
        if let l = limit { params["limit"] = l }
        return sendRaw(method: "agent.get_audit", params: params)
    }

    /// Record a client approval decision in the ptyd audit log.
    /// NOTE: this only persists the decision to the audit log (observe-model).
    /// To send the actual approval input to the agent (inject keystrokes into its pane),
    /// use pane.send_text / pane.send_bytes on the relevant pane_id — that is a
    /// follow-up step (ptyd pane.send_text) and is not handled here.
    @discardableResult
    public func approve(approvalID: String, decision: String, sessionID: String? = nil) -> String {
        var params: [String: Any] = [
            "approval_id": approvalID,
            "decision": decision
        ]
        if let sid = sessionID { params["session_id"] = sid }
        return sendRaw(method: "agent.approve", params: params)
    }

    // MARK: - Line I/O

    private func writeLine(_ line: String) {
        guard let conn = connection, let data = line.data(using: .utf8) else { return }
        conn.send(content: data, completion: .contentProcessed { [weak self] err in
            if let err {
                Task { @MainActor [weak self] in
                    self?.connectionState = .error(message: "Send error: \(err.localizedDescription)")
                }
            }
        })
    }

    private func scheduleRead() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 65536) { [weak self] data, _, isComplete, err in
            Task { @MainActor [weak self] in
                guard let self else { return }
                if let err {
                    self.connectionState = .error(message: "Receive error: \(err.localizedDescription)")
                    return
                }
                if let data, !data.isEmpty {
                    self.readBuffer.append(data)
                    self.drainLines()
                }
                if isComplete {
                    if case .connected = self.connectionState {
                        self.connectionState = .disconnected(reason: "Server closed connection")
                    }
                } else {
                    self.scheduleRead()
                }
            }
        }
    }

    /// Split accumulated buffer on `\n` boundaries; process each complete line.
    /// Partial trailing bytes (no newline yet) remain in readBuffer.
    private func drainLines() {
        while let newlineIdx = readBuffer.firstIndex(of: UInt8(ascii: "\n")) {
            let lineData = readBuffer[readBuffer.startIndex ..< newlineIdx]
            readBuffer = readBuffer[readBuffer.index(after: newlineIdx)...]
            if !lineData.isEmpty {
                handleLine(Data(lineData))
            }
        }
    }

    private func handleLine(_ data: Data) {
        // Decode as a generic PtydResponse to detect hello_ack vs normal response vs event.
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return
        }

        // If this carries an `id` — it's a response to one of our requests.
        if let resultObj = obj["result"] as? [String: Any],
           let type_ = resultObj["type"] as? String,
           type_ == "hello_ack" {
            // Hello ack — check ok field.
            let ok = resultObj["ok"] as? Bool ?? false
            if ok {
                connectionState = .connected
            } else {
                let errMsg = resultObj["error"] as? String ?? "auth rejected"
                connectionState = .error(message: "Hello rejected: \(errMsg)")
                connection?.cancel()
            }
            return
        }

        // All other lines — pass to the caller (SessionStore / subscriber).
        onLine?(data)
    }

    // MARK: - Helpers

    private func allocID() -> String {
        let id = "c\(nextID)"
        nextID += 1
        return id
    }

    private func sendRaw(method: String, params: [String: Any]) -> String {
        let id = allocID()
        let obj: [String: Any] = ["id": id, "method": method, "params": params]
        guard let data = try? JSONSerialization.data(withJSONObject: obj),
              let str = String(data: data, encoding: .utf8) else {
            return id
        }
        writeLine(str + "\n")
        return id
    }

    /// Public convenience for view-models that need to send arbitrary ptyd methods.
    /// Returns the request id (discardable).
    @discardableResult
    public func sendRawForUI(method: String, params: [String: Any]) -> String {
        sendRaw(method: method, params: params)
    }

    // MARK: - Fingerprint comparison (testable static)

    /// Constant-time-ish SHA-256 hex fingerprint comparison.
    /// Both strings are lowercased before comparison.
    /// nonisolated so it can be called from the Security verify block queue (non-main-actor).
    nonisolated static func fingerprintsMatch(_ a: String, _ b: String) -> Bool {
        let la = a.lowercased(), lb = b.lowercased()
        guard la.count == lb.count else { return false }
        // zip comparison — not constant-time at the Swift level, but the cert
        // fingerprint is not a secret the attacker can probe via timing over TLS.
        return la == lb
    }
}

// MARK: - ptyd wire request shapes

/// Top-level JSON-RPC request envelope.
private struct PtydRequest<P: Encodable>: Encodable {
    let id: String
    let method: String
    let params: P
}

/// Wrapper used only to extract id+method when we need a heterogeneous params dict.
private struct PtydRequestDynamic: Encodable {
    let id: String
    let method: String
}

/// Params for the `hello` method — matches mobile.rs `Hello` exactly.
struct HelloParams: Encodable {
    let protocolVersion: Int
    let token: String

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case token
    }
}
