// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use std::process::ExitCode;

mod args;
mod dispatch;
mod output;
mod query_commands;
mod resources;
mod runtime;
mod sync_commands;
mod trust_policy;
mod verify_commands;
mod write_commands;

fn main() -> ExitCode {
    match dispatch::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rally: {err}");
            ExitCode::FAILURE
        }
    }
}
