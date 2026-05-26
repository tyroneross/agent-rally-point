// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};

#[derive(Debug)]
pub struct CliError {
    pub command: &'static str,
    pub message: String,
    pub exit_code: u8,
    pub json: bool,
}

impl CliError {
    pub fn usage(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 2,
            json: false,
        }
    }

    pub fn runtime(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 1,
            json: false,
        }
    }

    pub fn not_found(command: &'static str, message: impl Into<String>) -> Self {
        Self {
            command,
            message: message.into(),
            exit_code: 3,
            json: false,
        }
    }

    pub fn emit(&self) {
        if self.json {
            eprintln!(
                "{}",
                json!({
                    "ok": false,
                    "command": self.command,
                    "error": self.message,
                    "exit_code": self.exit_code,
                })
            );
        } else {
            eprintln!("rally {}: {}", self.command, self.message);
        }
    }
}

#[derive(Debug)]
pub struct WriteOutput {
    pub json: bool,
    pub text: String,
    pub body: Value,
}

impl WriteOutput {
    pub fn emit(&self) {
        if self.json {
            println!("{}", self.body);
        } else {
            println!("{}", self.text);
        }
    }
}
