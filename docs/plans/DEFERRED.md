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
| **Multi-user / zero-knowledge relay** | `owner_id` in schema + `Transport` trait seam only | Implement the relay + per-session wrapped-key E2E crypto when going multi-user (see spec §8). |

## [CLEANUP] dev shortcuts introduced (must close before any "production" claim)

| Shortcut | Why | Verify-gone condition |
|---|---|---|
| `COCKPIT_TOKEN` dev bearer auth | No device for SE-mTLS in autonomous build | Replaced by `MtlsAuth` as the default `AuthProvider`. |
| Mock `claude`/`codex` CLIs in tests | No live-credit burn over 24h | Live smoke passes against real CLIs under `COCKPIT_LIVE=1`. |
| Daemon binds loopback only | No tailnet in build env | Bind addr is the tailnet interface in deployed config. |
| Face ID stub flag in iOS | Simulator can't do biometrics | `LAContext` path active on device. |
