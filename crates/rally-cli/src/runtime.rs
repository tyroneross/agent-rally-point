// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, SecondsFormat, Utc};
use rally_protocol::sha256_hash;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static FALLBACK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn now_rfc3339() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn new_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return format!("{prefix}_{}", hex_bytes(&bytes));
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let counter = FALLBACK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{prefix}:{nanos}:{}:{counter}", std::process::id());
    let hash = sha256_hash(seed.as_bytes());
    format!("{prefix}_{}", &hash["sha256:".len().."sha256:".len() + 32])
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}
