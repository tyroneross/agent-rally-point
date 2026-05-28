use serde_json::{Value, json};

use crate::error::RallyError;

pub(crate) struct Output {
    pub(crate) json: bool,
    pub(crate) text: String,
    pub(crate) body: Value,
    pub(crate) exit_code: u8,
}

impl Output {
    pub(crate) fn new(json: bool, text: String, body: Value) -> Self {
        Self {
            json,
            text,
            body,
            exit_code: 0,
        }
    }

    pub(crate) fn with_exit_code(mut self, exit_code: u8) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub(crate) fn print(self) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&self.body).unwrap_or(self.body.to_string())
            );
        } else {
            println!("{}", self.text);
        }
    }
}

pub(crate) struct CliError {
    pub(crate) message: String,
    pub(crate) exit_code: u8,
    pub(crate) json: bool,
}

impl CliError {
    pub(crate) fn from_error(error: RallyError, json: bool) -> Self {
        Self {
            exit_code: error.exit_code(),
            message: error.to_string(),
            json,
        }
    }

    pub(crate) fn print(&self) {
        if self.json {
            eprintln!(
                "{}",
                json!({
                    "ok": false,
                    "product": "rally",
                    "error": self.message,
                    "exit_code": self.exit_code
                })
            );
        } else {
            eprintln!("rally: {}", self.message);
        }
    }
}
