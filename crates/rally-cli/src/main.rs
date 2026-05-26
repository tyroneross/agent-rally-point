// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::{event_id, read_jsonl};
use rally_trust::{PublicKeyStore, TrustStatus, classify};
use std::collections::BTreeMap;
use std::env;
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
    match (args.next().as_deref(), args.next()) {
        (Some("verify"), Some(path)) => {
            verify(&path)?;
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            eprintln!("usage: rally-rs verify <changes.jsonl>");
            Ok(ExitCode::from(2))
        }
    }
}

fn verify(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let records = read_jsonl(path)?;
    let keys = PublicKeyStore::new();
    let mut counts: BTreeMap<TrustStatus, usize> = BTreeMap::new();

    for record in &records {
        let classification = classify(record, &keys)?;
        *counts.entry(classification.status).or_default() += 1;
        let id = event_id(record).unwrap_or_else(|_| "<missing-id>".to_string());
        println!("{id} {}", classification.status);
    }

    print!("summary records={}", records.len());
    for (status, count) in counts {
        print!(" {status}={count}");
    }
    println!();
    Ok(())
}
