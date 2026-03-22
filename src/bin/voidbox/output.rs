use std::fmt;

use serde::Serialize;

/// Output format for CLI commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputFormat::Human => write!(f, "human"),
            OutputFormat::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "human" => Ok(OutputFormat::Human),
            "json" => Ok(OutputFormat::Json),
            other => Err(format!(
                "unknown output format '{other}', expected human|json"
            )),
        }
    }
}

/// Print a serializable value to stdout as JSON (pretty) or human table.
pub fn print_json_or_human<T: Serialize>(
    format: OutputFormat,
    value: &T,
    human_fn: impl FnOnce(&T),
) {
    match format {
        OutputFormat::Human => human_fn(value),
        OutputFormat::Json => match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("error: failed to serialize JSON output: {e}"),
        },
    }
}

/// Structured error for JSON output on stderr.
#[derive(Debug, Serialize)]
pub struct CliError {
    pub error: CliErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct CliErrorDetail {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl CliError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            error: CliErrorDetail {
                message: message.into(),
                code: None,
            },
        }
    }

    #[allow(dead_code)]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.error.code = Some(code.into());
        self
    }
}

/// Report an error to stderr, respecting output format.
pub fn report_error(format: OutputFormat, err: &dyn std::error::Error) {
    match format {
        OutputFormat::Human => eprintln!("error: {err}"),
        OutputFormat::Json => {
            let cli_err = CliError::new(err.to_string());
            if let Ok(json) = serde_json::to_string_pretty(&cli_err) {
                eprintln!("{json}");
            }
        }
    }
}

/// Format a structured JSON value for CLI display (pretty-print; fallback to compact).
pub fn format_json_value(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Print a [`serde_json::Value`] to stdout respecting [`OutputFormat`] (e.g. daemon responses).
pub fn print_json_value(format: OutputFormat, value: &serde_json::Value) {
    print_json_or_human(format, value, |v| {
        println!("{}", format_json_value(v));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_parses_human_and_json_case_insensitive() {
        assert_eq!(
            "human".parse::<OutputFormat>().unwrap(),
            OutputFormat::Human
        );
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
    }

    #[test]
    fn output_format_rejects_unknown() {
        assert!("yaml".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn format_json_value_pretty_prints() {
        let v = serde_json::json!({"a": 1});
        let s = format_json_value(&v);
        assert!(s.contains('\n') || s.contains('1'));
        assert!(s.contains("a"));
    }
}
