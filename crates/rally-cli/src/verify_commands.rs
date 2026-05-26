// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use crate::trust_policy::load_optional_trust;
use rally_core::store::load_records;
use rally_protocol::event_id;
use rally_trust::{PublicKeyStore, TrustStatus, classify, classify_with_policy};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct VerifyOptions {
    pub(crate) path: String,
    pub(crate) json: bool,
    trust_policy: Option<PathBuf>,
    no_default_trust_policy: bool,
}

impl VerifyOptions {
    pub(crate) fn parse(
        args: impl Iterator<Item = String>,
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let mut options = Self {
            path: String::new(),
            json: false,
            trust_policy: None,
            no_default_trust_policy: false,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => options.json = true,
                "--no-default-trust-policy" => options.no_default_trust_policy = true,
                "--trust-policy" => {
                    let Some(path) = args.next() else {
                        return Err("--trust-policy requires a path".into());
                    };
                    options.trust_policy = Some(PathBuf::from(path));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option {value}").into());
                }
                value => {
                    if !options.path.is_empty() {
                        return Err(format!("unexpected extra argument {value}").into());
                    }
                    options.path = value.to_string();
                }
            }
        }
        Ok((!options.path.is_empty()).then_some(options))
    }
}

pub(crate) fn verify(options: &VerifyOptions) -> Result<(), Box<dyn std::error::Error>> {
    let records = load_records(&options.path)?;
    let trust = load_optional_trust(
        options.trust_policy.as_ref(),
        options.no_default_trust_policy,
    )?;
    let mut counts: BTreeMap<TrustStatus, usize> = BTreeMap::new();
    let mut json_events = Vec::new();

    for record in &records {
        let classification = if let Some(context) = trust.as_ref() {
            classify_with_policy(record, &context.keys, Some(&context.policy))?
        } else {
            classify(record, &PublicKeyStore::new())?
        };
        *counts.entry(classification.status).or_default() += 1;
        let id = event_id(record).unwrap_or_else(|_| "<missing-id>".to_string());
        if options.json {
            json_events.push(json!({
                "id": id,
                "status": classification.status,
                "key_id": classification.key_id,
            }));
        } else {
            let key = classification
                .key_id
                .as_deref()
                .map(|key_id| format!(" key_id={key_id}"))
                .unwrap_or_default();
            println!("{id} {}{key}", classification.status);
        }
    }

    if options.json {
        let trust_policy = trust.and_then(|context| context.source);
        println!(
            "{}",
            json!({
                "ok": true,
                "command": "verify",
                "schema": "agent-rally.command.verify.v1",
                "data": {
                    "records": records.len(),
                    "trust_policy": trust_policy,
                    "counts": counts,
                    "events": json_events,
                }
            })
        );
    } else {
        print!("summary records={}", records.len());
        for (status, count) in counts {
            print!(" {status}={count}");
        }
        println!();
    }
    Ok(())
}
