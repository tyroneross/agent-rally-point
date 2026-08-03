# Deferred surfaces & cleanup register

Everything the **autonomous build cannot verify** because it needs a physical
device, an Apple Developer account, a Tailscale tailnet, or live vendor credit.
Each item is `TAG:UNTESTED` and lists the exact human step to finish it. Nothing
here is claimed working.

## Device / account / network gated (NEEDS-HUMAN)

| Item | State after autonomous build | Human step to finish |
|---|---|---|
| **Tailscale tailnet transport** | Daemon binds `127.0.0.1`; `Transport` trait + tailnet-binding stub present | Install Tailscale on the Mac + iPhone, join one tailnet, set the daemon bind addr to the tailnet interface, add an ACL grant restricting to your devices. |
| **Secure-Enclave mTLS** | Dev bearer-token auth (`COCKPIT_TOKEN`); `AuthProvider` trait with a `DevTokenAuth` impl + an `MtlsAuth` stub | On a physical iPhone: generate a non-exportable SE key, issue a client cert, implement the mTLS handshake on both ends, gate behind Face ID. |
| **Face ID per-action gate** | Stubbed behind a dev-mode flag in the iOS approval UI | Wire `LocalAuthentication` (`LAContext`) on a real device; require biometric re-auth before approve/destructive actions. |
| **APNs push (approvals/finished)** | Payload shape + registration scaffold; no delivery | Apple Developer account → APNs key, push entitlement, register device token, send visible push for approvals + silent push to wake/resync. |
| **CloudKit history/settings sync** | Not built (P3) | Add CloudKit entitlement + container; mirror session history/settings to the private DB. |
| **launchctl load of cockpitd** | plist lints; `install.sh` parses | Run `deploy/install.sh install` on the target Mac (needs a login session). |
| **Live agent adapters** | Verified against MOCK `claude`/`codex`; real CLIs present (claude 2.1.158, codex 0.130.0) | Run the gated smoke (`COCKPIT_LIVE=1 cargo test -p cockpitd -- --ignored live`) against the real CLIs (burns credit). |
| **Multi-user / zero-knowledge relay** | **BUILT + tested in-process** — `crypto.rs` (Ed25519 identity/challenge-response, X25519-wrapped AES/secretbox per-session keys), `transport/relay.rs` `ZeroKnowledgeRelay` (proven ciphertext-only), `owner_id` isolation in the store. | Only the *hosted deployment* remains: run the relay as a reachable service, wire device pairing/QR, and the app-layer E2E on the wire (the primitives are done). |

## Built since the MVP (no longer deferred)

These were "seams" in the original plan and are now real, tested code:
- **Append-only audit log** (`audit.rs`) — commands/approvals/lifecycle, `get_audit` wire cmd.
- **Advisory command-approval surface** (`authz.rs` + the review loop in `run_pump`) — non-allowlisted tool_calls and Codex native approvals surface for per-session approval. **This is not an execution control.** The gate sees a `tool_call` only after it appears in the child's event stream, and pausing the event pump does not pause the child process — the tool has already run, or may run regardless of the decision. Denial stops the result being forwarded to the UI; it does not stop the tool. See ARP-003 / RC-015; the real broker redesign is still outstanding.
- **Multi-user crypto + zero-knowledge relay** (`crypto.rs`, `transport/relay.rs`) — see row above.
- **WS-level approval round-trip** with TTL/auto-deny logic (`approval.rs`).

## Small software items still open (verifiable, not hardware-gated)

- **TTL auto-deny background task** — `ApprovalManager.sweep()` exists + is unit-tested, but no daemon task calls it periodically (and it must also wake parked gates with a deny). One focused chunk.
- **Live app↔daemon UI E2E** — a simulator UITest driving a running `cockpitd`; deferred for flakiness, `cockpit-cli` E2E is the verified stand-in.

## [CLEANUP] dev shortcuts introduced (must close before any "production" claim)

| Shortcut | Why | Verify-gone condition |
|---|---|---|
| `COCKPIT_TOKEN` dev bearer auth | No device for SE-mTLS in autonomous build | Replaced by `MtlsAuth` as the default `AuthProvider`. |
| Mock `claude`/`codex` CLIs in tests | No live-credit burn over 24h | Live smoke passes against real CLIs under `COCKPIT_LIVE=1`. |
| Daemon binds loopback only | No tailnet in build env | Bind addr is the tailnet interface in deployed config. |
| Face ID stub flag in iOS | Simulator can't do biometrics | `LAContext` path active on device. |
