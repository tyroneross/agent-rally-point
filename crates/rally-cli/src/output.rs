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
        self.render().print();
    }

    /// Pre-render to a plain `RenderedOutput` so the bytes can be moved across
    /// a thread boundary and printed later (used by the wall-clock watchdog in
    /// `main`, where computation happens on a worker thread but printing must
    /// happen on the main thread after the deadline check).
    pub(crate) fn render(self) -> RenderedOutput {
        let stdout = if self.json {
            json_string(&self.body)
        } else {
            self.text
        };
        RenderedOutput {
            stdout,
            stderr: None,
        }
    }
}

/// Output already serialized to its final string form, decoupled from where it
/// is printed. Cheap to send across a channel.
pub(crate) struct RenderedOutput {
    stdout: String,
    stderr: Option<String>,
}

impl RenderedOutput {
    pub(crate) fn print(self) {
        if let Some(err) = self.stderr {
            eprintln!("{err}");
        } else {
            println!("{}", self.stdout);
        }
    }
}

fn json_string(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("serde_json::Value serialization is infallible")
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
        eprintln!("{}", self.error_text());
    }

    /// Pre-render the error to a `RenderedOutput` (always on stderr) for the
    /// watchdog's cross-thread path. Mirrors [`CliError::print`].
    pub(crate) fn render_err(&self) -> RenderedOutput {
        RenderedOutput {
            stdout: String::new(),
            stderr: Some(self.error_text()),
        }
    }

    fn error_text(&self) -> String {
        if self.json {
            json!({
                "ok": false,
                "product": "rally",
                "error": self.message,
                "exit_code": self.exit_code
            })
            .to_string()
        } else {
            format!("rally: {}", self.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::json_string;
    use serde_json::json;

    #[test]
    fn json_output_uses_pretty_serialization() {
        assert_eq!(json_string(&json!({"ok": true})), "{\n  \"ok\": true\n}");
    }
}
