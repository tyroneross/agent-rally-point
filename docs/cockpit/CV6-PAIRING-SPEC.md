# CV6 — Automated pairing (spec)

**Status:** design / pre-implementation. **Date:** 2026-05-31.
**Cross-refs:** `easy-terminal/docs/SECURE-MOBILE.md` (transport + D1/D2 decisions),
`docs/cockpit/CONVERGENCE.md` (ptyd is the one daemon), `ptyd/src/mobile.rs`
(committed auth core), `ptyd/docs/CLIENT-API.md` (the `hello` handshake).

## 1. Goal (grounded in the app)

Agent Cockpit lets the user drive Claude/Codex sessions on their always-on Mac
(ptyd daemon) from their iPhone. Today the iOS app must be handed a 64-char
pairing **token** + the daemon's self-signed cert **SHA-256 fingerprint**,
typed or pasted into Settings. That is the one piece of real friction in the
first-run experience.

**CV6 goal:** make first-run pairing *near-zero-touch* and "default sign-in"-like
for the user's own same-Apple-ID devices — **without** sacrificing a deterministic,
offline, no-iCloud fallback, and **without** claiming security properties we can't
verify. Pairing must remain consistent with the project's invariants:
- **Residency:** the pairing secret rests only on the phone, the Mac, the user's
  iCloud, or in transit to the daemon — never a third-party server.
- **Trust model (unchanged):** the cert-fingerprint **pin** + the **token** are
  what authenticate the TLS `hello`. Pairing only *delivers* those two values to
  the phone; it does not change what authenticates the connection.
- **Daemon stays Apache/std-only:** the Rust daemon (`ptyd`) gains nothing here.
  All Apple-platform work lives in the Swift Mac app (the bridge) + the iOS app.

## 2. Why this design (research-grounded)

Web research into shipping apps + Apple Developer Forums (sources in §8) produced
hard constraints that **rule out a single-mechanism design**:

- **iCloud Keychain sync is non-deterministic.** Apple DTS on the forums: sync can
  take "<1 min to >1 hour"; there is **no force/poll API**. The documented user fix
  for stuck sync is literally "toggle iCloud Keychain off/on." → cannot be the sole
  path; cannot block first connection on it.
- **No API to detect iCloud Keychain is disabled.** The item exists locally and
  silently never propagates. → must detect heuristically + fall back.
- **macOS needs `kSecUseDataProtectionKeychain = true`** or synchronizable items
  silently use the legacy keychain and never sync (undocumented; Square's Valet got
  bitten). → set from day one.
- **CloudKit private DB is *not* E2E by default, and there's no API to check ADP**,
  plus the **dev-schema ≠ prod-schema** trap silently breaks sync after App Store
  release. → don't use CloudKit for the secret; don't claim E2E we can't verify.
- **Passkeys require a real public domain + AASA file** — a local daemon can't be a
  WebAuthn relying party. → non-starter (see SECURE-MOBILE; revisit only with a
  public hostname).
- **The closest real-world analog avoided keychain sync.** Raivo OTP syncs TOTP
  secrets across Apple devices but, for the **Mac↔iOS** path, uses an **APNS push**
  as the transport (with its own E2E layer) rather than relying on keychain sync.
  Blink Shell pairs a second device via **QR**. SSH/remote apps (Prompt, Termius,
  Screens) keep credentials local + biometric-gated and sync only metadata.

**Conclusion:** a **three-tier** design, deterministic-primary, convenience-layered.

## 3. The design — three tiers

### Tier A · QR / manual — the deterministic primary  (build first)
The reliable path; works offline, no iCloud, no Apple Developer account.
- **Mac app** renders a QR encoding the pairing payload (§4). It already has the
  values (reads ptyd's token from its state dir; computes the cert fingerprint, or
  reads it from `ptyd status server --json`'s `tls_fingerprint`).
- **iOS app** scans the QR (AVFoundation) → fills `CockpitConfig` (host, port,
  token, fingerprint) → connects. Manual copy/paste remains as the ultimate fallback
  (today's Settings fields stay).
- This is what the user sees on day one and whenever iCloud isn't available.

### Tier B · iCloud Keychain auto-fill — best-effort convenience  (build second)
Zero-touch on the user's *other* same-Apple-ID devices, layered on Tier A.
- **Mac app** writes ONE `kSecClassGenericPassword` item: `kSecAttrSynchronizable =
  true`, `kSecUseDataProtectionKeychain = true`, a shared `kSecAttrAccessGroup`
  (`<TeamID>.ai.rosslabs.cockpit.pairing`). Value = the pairing payload (§4).
  iCloud Keychain items are **E2E by default** (unlike CloudKit) and residency-clean.
- **iOS app** reads that item on launch / when unpaired. If present → auto-fill →
  connect. Never blocks: if absent, fall straight through to Tier A's UI.
- Only `kSecClassGenericPassword`/`InternetPassword` sync — so the cert fingerprint
  is stored *inside* the payload blob, not as a `kSecClassCertificate` item.

### Tier C · APNS push-to-read nudge — deterministic zero-touch  (optional, later)
Solves Tier B's "did it sync yet / arbitrary delay" problem, and is the Raivo model
adapted. Rides the APNs work already on the ptyd/Cockpit roadmap.
- After the Mac app writes the keychain item (Tier B), it sends a **silent push** to
  the user's paired iPhone: *"pairing updated — read now."* The iOS app wakes, reads
  the keychain item, connects. The **secret never travels in the push** (APNs is not
  E2E) — the push is only a *trigger*; the E2E keychain item carries the secret. This
  is exactly Apple DTS's recommended hybrid (keychain distributes the secret; a more
  observable channel signals readiness).
- Requires an Apple Developer account + APNs key + push entitlement (gated).

## 4. Pairing payload (shared schema)

One JSON object, used identically by the QR encoder and the keychain blob:
```jsonc
{ "v": 1,
  "host": "100.x.y.z",        // tailnet/loopback addr the daemon's TLS listener binds
  "port": 8443,
  "token": "<64-hex pairing token>",   // from ptyd state dir
  "fp": "<64-hex SHA-256 cert fingerprint>" }  // from ptyd status server --json
```
The iOS app maps this straight into `CockpitConfig` (host/port/pairingToken/
pinnedFingerprint — the fields CV5 already added to Settings).

## 5. Pairing state machine (iOS) — handle the failure modes explicitly

`unpaired → pairing(source: qr|keychain|push) → verifying → paired → error(reason)`
- **`verifying` is the real confirmation** — pairing is "done" only after a
  successful authenticated TLS `hello` round-trip to the daemon (cert pin + token
  accepted). Keychain *sync propagation is NOT acknowledgment* — never show "paired"
  on a keychain read alone; always verify by connecting.
- **iCloud-Keychain-off / not-synced detection (heuristic, no API exists):** on the
  Mac side, after writing, optionally read back; on the iOS side, if unpaired and no
  keychain item appears within a short grace window, surface *"Couldn't auto-pair —
  scan the QR from Easy Terminal, or check iCloud Keychain is on."* Never spin
  forever.
- **Re-pair / rotation:** tokens are per-device and revocable (SECURE-MOBILE D2). A
  "Re-pair" action re-reads/re-scans; revoking on the daemon invalidates the token so
  a lost device drops.

## 6. What to copy / what to avoid (from prior art, §8)

**Copy:** manual QR/token as a co-primary (Blink); keychain-distributes-secret +
push-signals-readiness hybrid (Apple DTS); read-back-confirm by actually connecting;
`kSecUseDataProtectionKeychain` + explicit access group + Keychain-Sharing
entitlement on **every** target from day one; an explicit named sync/pairing state
(better than Bear's "red-dot" minimum).

**Avoid:** treating keychain sync as acknowledgment; assuming any sync window;
CloudKit for the secret (not E2E by default; prod-schema trap); rendering "E2E
encrypted" we can't verify (no ADP-status API); shipping without the deterministic
fallback.

## 7. Deliverables / phasing

| Phase | Scope | Verifiable in build | `TAG:UNTESTED` (needs hardware) |
|---|---|---|---|
| **CV6-A** | Mac app QR generator + iOS QR scanner + payload schema; map → CockpitConfig | iOS: scanner→config unit tests, payload decode; Mac: QR-payload encode tests | camera scan on a real device |
| **CV6-B** | iCloud Keychain publish (Mac) + read (iOS); `kSecUseDataProtectionKeychain`, shared access group; off-detection heuristic; state machine | keychain read/write/round-trip unit tests on sim; state-machine tests | cross-device sync (2 same-Apple-ID devices) |
| **CV6-C** *(opt)* | APNs push-to-read nudge (Mac sends, iOS wakes+reads); push carries NO secret | payload/trigger unit tests | live APNs (dev account + key + device) |

Each phase: simulator-buildable + unit-tested; the genuinely device/account-gated
bits are flagged, not faked (consistent with `DEFERRED.md`).

## 8. Risks

1. **iCloud Keychain reliability** — mitigated by QR-primary + never-block + Tier-C
   push nudge.
2. **Same Team ID requirement** — the Easy Terminal Mac app and the Cockpit iOS app
   must ship under one Apple Developer Team for the shared access group. (They will,
   as one product.)
3. **APNs gating** — Tier C needs an Apple Developer account + key; deferred.
4. **Chicken-and-egg clarity** — QR/keychain/push only *deliver* the token+fingerprint;
   the cert pin + token are the actual auth. Keep that boundary explicit so pairing
   convenience never becomes a trust shortcut.

## 9. Success criteria

- First run: **scan one QR → connected, zero typing.**
- A second same-Apple-ID device: app **auto-fills from keychain (best-effort)** or is
  **push-nudged (Tier C)** and connects with no QR.
- iCloud Keychain off / sync delayed → app **still pairs via QR** and shows a clear,
  bounded state — never a false "paired," never a claim of E2E it can't verify.

## Sources (research, 2026-05-31)
Apple DTS on non-deterministic keychain sync (Dev Forums 727073); keychain-stops-
syncing fix (Forums 40603); `kSecUseDataProtectionKeychain` requirement (Square Valet
PR #193); CloudKit dev≠prod schema (fatbobman, leojkwan); CloudKit/ADP-not-E2E + no
status API (Tact blog, Apple Security Guide); Raivo OTP APNS Mac↔iOS pairing (GitHub);
Blink Shell QR/WebAuthn (Blink docs); Damian Mehers `kSecAttrSynchronizable` cross-
platform walkthrough. Full URLs in the CV6 research note / conversation log.
