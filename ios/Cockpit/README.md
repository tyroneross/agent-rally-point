# Cockpit (iOS)

Native iOS surface for **Agent Cockpit** — drive Claude Code + Codex sessions
running on an always-on Mac from your phone. This is the client; the Mac-side
`cockpitd` daemon (see `crates/cockpitd`) is the server.

## What it does (v1)

- Lists all Claude/Codex sessions on the Mac with live status.
- Per session: read the structured timeline (messages, tool calls, diffs), send a
  prompt, steer mid-run, and approve/deny tool calls.
- Launch a new session ("new window") for either agent against a repo path.
- Reconnect-and-replay: backgrounding the app is treated as a disconnect; on
  return it resyncs from the last seen `seq` with no gaps or dupes.

## Layout

```
ios/Cockpit/
  project.yml                      # xcodegen project definition (source of truth)
  Sources/CockpitApp/              # @main app shell + Info.plist
  Sources/Cockpit/
    Models/WireModels.swift        # Codable mirror of docs/plans/COCKPIT-WIRE.md
    Net/CockpitClient.swift        # URLSessionWebSocketTask client
    Net/ResyncMachine.swift        # seq-cursor reconnect/replay state machine
    ViewModels/                    # SessionStore + per-screen view-models
    Views/                         # SwiftUI: list, detail/timeline, composer, launcher, approval
  Tests/CockpitTests/              # wire-decode + resync unit tests
```

## Build & test

The `.xcodeproj` is **generated** (gitignored). Regenerate it, then build/test on
a simulator:

```bash
cd ios/Cockpit
xcodegen generate
xcodebuild -project Cockpit.xcodeproj -scheme Cockpit \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

## Connecting to the daemon

Set the daemon WebSocket URL and dev token in the app config, then point it at
your Mac. Over localhost (simulator) it's `ws://127.0.0.1:8787`; on a real device
it's the daemon's address on your Tailscale tailnet. See `crates/cockpitd` for
running the daemon and `docs/plans/COCKPIT-WIRE.md` for the protocol.

## Not yet wired (see `docs/plans/DEFERRED.md`)

Secure-Enclave mTLS, Face ID gating (stubbed dev-mode — needs a physical device),
APNs push, and CloudKit sync are scaffolded/deferred. v1 auth is a dev bearer
token; the real device/account-gated work is enumerated in DEFERRED.md.
