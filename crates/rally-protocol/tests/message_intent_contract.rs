// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::{
    ActorKind, AuthorityBasis, Directive, MessageContext, MessageIntent, RoomSeat,
    WorkResponsibility,
};

#[test]
fn legacy_directive_defaults_to_controlling_unknown_context() {
    let legacy = r#"{
        "seq": 1,
        "to": "codex:worker",
        "from": "claude_code:lead",
        "kind": "deliver",
        "type": "addition",
        "text": "do the work",
        "urgent": false,
        "ts": 1.0
    }"#;

    let directive: Directive = serde_json::from_str(legacy).expect("legacy directive decodes");
    assert_eq!(directive.message.intent, MessageIntent::Directive);
    assert!(directive.message.intent.is_controlling());
    assert_eq!(directive.message.actor_kind, ActorKind::Unknown);
    assert_eq!(directive.message.room_seat, RoomSeat::Unknown);
    assert_eq!(
        directive.message.authority_basis,
        AuthorityBasis::Unverified
    );
    assert_eq!(
        directive.message.responsibility,
        WorkResponsibility::Unspecified
    );
}

#[test]
fn unknown_future_intent_decodes_and_fails_closed() {
    let raw = r#"{
        "intent": "escalate",
        "actor_kind": "agent",
        "room_seat": "participant",
        "responsibility": "investigator",
        "authority_basis": "not_required"
    }"#;

    let context: MessageContext = serde_json::from_str(raw).expect("future intent decodes");
    assert_eq!(context.intent, MessageIntent::Unknown);
    assert!(context.intent.is_controlling());
}

#[test]
fn non_controlling_intents_are_an_explicit_closed_set() {
    for intent in [
        MessageIntent::Inform,
        MessageIntent::Request,
        MessageIntent::Propose,
    ] {
        assert!(
            !intent.is_controlling(),
            "{intent:?} must not grant control"
        );
    }
    for intent in [MessageIntent::Directive, MessageIntent::Unknown] {
        assert!(intent.is_controlling(), "{intent:?} must fail closed");
    }
}

#[test]
fn responsibility_and_room_seat_are_independent() {
    let context = MessageContext {
        intent: MessageIntent::Request,
        actor_kind: ActorKind::Agent,
        caller_session_id: Some("sess:codex:01#live".into()),
        room_seat: RoomSeat::Participant,
        lead_epoch: Some(42),
        responsibility: WorkResponsibility::Integrator,
        authority_basis: AuthorityBasis::NotRequired,
    };

    let value = serde_json::to_value(&context).unwrap();
    assert_eq!(value["room_seat"], "participant");
    assert_eq!(value["responsibility"], "integrator");
    assert_eq!(value["intent"], "request");
    assert_eq!(value["authority_basis"], "not_required");
    assert!(value.get("peer").is_none());
}

#[test]
fn legacy_sender_session_key_decodes_as_unbound_caller_session() {
    let context: MessageContext = serde_json::from_value(serde_json::json!({
        "sender_session_id": "sess:legacy:01#live"
    }))
    .expect("legacy session key decodes");
    assert_eq!(
        context.caller_session_id.as_deref(),
        Some("sess:legacy:01#live")
    );

    let encoded = serde_json::to_value(&context).expect("new session key encodes");
    assert_eq!(
        encoded["caller_session_id"],
        serde_json::json!("sess:legacy:01#live")
    );
    assert!(encoded.get("sender_session_id").is_none());
}
