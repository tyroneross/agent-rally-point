// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import Foundation

// Deferred-surface scaffold (chunk E2): APNs push for "needs approval / finished"
// events. This is a COMPILING STUB that documents the payload + registration
// shape. It is intentionally non-functional — real delivery needs an Apple
// Developer account, an APNs key, the push entitlement, and a physical device.
// APNs registration remains intentionally scaffolded until the runtime path is wired.
//
// TAG:UNTESTED — APNs end-to-end cannot be verified in the autonomous build.

/// The two push categories the daemon will send.
enum CockpitPushKind: String, Codable {
    /// Visible alert push — reliable delivery for must-not-miss approvals.
    case approvalRequest = "approval_request"
    /// Silent/background push (content-available:1) to wake the app and resync.
    case sessionUpdate = "session_update"
}

/// Decoded shape of a Cockpit push payload's custom keys (alongside `aps`).
struct CockpitPushPayload: Codable {
    let kind: CockpitPushKind
    let sessionId: String
    /// Present for `approvalRequest`.
    let approvalId: String?
    /// Short human summary for the alert body.
    let summary: String?
}

/// Registration seam. On a real device this would request authorization, call
/// `UIApplication.shared.registerForRemoteNotifications()`, and POST the returned
/// device token to the daemon so it can target this device. Here it only records
/// intent so the call sites exist and compile.
enum ApnsRegistration {
    /// What the app must do on first launch (documented, not executed here).
    static let requiredEntitlement = "aps-environment"

    /// TAG:UNTESTED — returns nil; the real token comes from
    /// `didRegisterForRemoteNotificationsWithDeviceToken` on a device.
    static func currentDeviceToken() -> String? { nil }
}
