// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::{event_id, read_jsonl};
use rally_trust::{
    PublicKeyStore, TrustContext, TrustPolicy, TrustStatus, classify, classify_with_policy,
    load_trust_file,
};
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rally-rs: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("verify") => {
            let Some(options) = VerifyOptions::parse(args)? else {
                usage();
                return Ok(ExitCode::from(2));
            };
            verify(options)?;
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            usage();
            Ok(ExitCode::from(2))
        }
    }
}

#[derive(Debug)]
struct VerifyOptions {
    path: String,
    json: bool,
    trust_policy: Option<PathBuf>,
    no_default_trust_policy: bool,
}

impl VerifyOptions {
    fn parse(
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

fn usage() {
    eprintln!(
        "usage: rally-rs verify [--json] [--trust-policy <trust.toml>] [--no-default-trust-policy] <changes.jsonl>"
    );
}

fn verify(options: VerifyOptions) -> Result<(), Box<dyn std::error::Error>> {
    let records = read_jsonl(&options.path)?;
    let trust = load_trust_context(&options)?;
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
            json_events.push(serde_json::json!({
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
            serde_json::json!({
                "records": records.len(),
                "trust_policy": trust_policy,
                "counts": counts,
                "events": json_events,
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

struct LoadedTrust {
    keys: PublicKeyStore,
    policy: TrustPolicy,
    source: Option<String>,
}

fn load_trust_context(
    options: &VerifyOptions,
) -> Result<Option<LoadedTrust>, Box<dyn std::error::Error>> {
    let Some(path) = trust_policy_path(options) else {
        return Ok(None);
    };
    if options.trust_policy.is_none() && !path.exists() {
        return Ok(None);
    }
    let TrustContext { keys, policy } = load_trust_file(&path)?;
    Ok(Some(LoadedTrust {
        keys,
        policy,
        source: Some(path.display().to_string()),
    }))
}

fn trust_policy_path(options: &VerifyOptions) -> Option<PathBuf> {
    if let Some(path) = options.trust_policy.clone() {
        return Some(path);
    }
    if options.no_default_trust_policy {
        return None;
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity/trust.toml"))
}
