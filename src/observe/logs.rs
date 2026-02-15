//! Structured Logging with Correlation IDs
//!
//! Provides structured logging for workflow execution:
//! - Log levels (trace, debug, info, warn, error)
//! - Correlation IDs for request tracing
//! - Structured key-value attributes
//! - stdout/stderr capture from sandboxed processes

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Log levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Trace => write!(f, "TRACE"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// Configuration for structured logging
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Enable logging
    pub enabled: bool,
    /// Minimum log level
    pub level: LogLevel,
    /// Include stdout from processes
    pub include_stdout: bool,
    /// Include stderr from processes
    pub include_stderr: bool,
    /// Maximum entries to keep in memory
    pub max_entries: usize,
    /// Enable in-memory collection (for testing)
    pub in_memory: bool,
    /// Output to tracing crate
    pub output_to_tracing: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: LogLevel::Info,
            include_stdout: true,
            include_stderr: true,
            max_entries: 10000,
            in_memory: false,
            output_to_tracing: true,
        }
    }
}

impl LogConfig {
    /// Create a config for in-memory logging (useful for tests)
    pub fn in_memory() -> Self {
        Self {
            enabled: true,
            level: LogLevel::Trace,
            in_memory: true,
            output_to_tracing: false,
            ..Default::default()
        }
    }

    /// Set the log level
    pub fn level(mut self, level: LogLevel) -> Self {
        self.level = level;
        self
    }

    /// Enable or disable stdout capture
    pub fn include_stdout(mut self, enable: bool) -> Self {
        self.include_stdout = enable;
        self
    }

    /// Enable or disable stderr capture
    pub fn include_stderr(mut self, enable: bool) -> Self {
        self.include_stderr = enable;
        self
    }
}

/// A structured log entry
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Timestamp
    pub timestamp: SystemTime,
    /// Log level
    pub level: LogLevel,
    /// Log message
    pub message: String,
    /// Trace ID for correlation
    pub trace_id: Option<String>,
    /// Span ID for correlation
    pub span_id: Option<String>,
    /// Additional attributes
    pub attributes: HashMap<String, String>,
    /// Source (e.g., "stdout", "stderr", "workflow")
    pub source: String,
}

impl LogEntry {
    /// Create a new log entry
    pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
        Self {
            timestamp: SystemTime::now(),
            level,
            message: message.into(),
            trace_id: None,
            span_id: None,
            attributes: HashMap::new(),
            source: "workflow".to_string(),
        }
    }

    /// Set the trace ID
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Set the span ID
    pub fn with_span_id(mut self, span_id: impl Into<String>) -> Self {
        self.span_id = Some(span_id.into());
        self
    }

    /// Add an attribute
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Set the source
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Format as JSON
    pub fn to_json(&self) -> String {
        let timestamp = self
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        let mut json = format!(
            r#"{{"timestamp":{},"level":"{}","message":"{}","source":"{}""#,
            timestamp,
            self.level,
            self.message.replace('\\', "\\\\").replace('"', "\\\""),
            self.source
        );

        if let Some(ref trace_id) = self.trace_id {
            json.push_str(&format!(r#","trace_id":"{}""#, trace_id));
        }

        if let Some(ref span_id) = self.span_id {
            json.push_str(&format!(r#","span_id":"{}""#, span_id));
        }

        if !self.attributes.is_empty() {
            json.push_str(r#","attributes":{"#);
            let attrs: Vec<_> = self
                .attributes
                .iter()
                .map(|(k, v)| {
                    format!(
                        r#""{}":"{}""#,
                        k,
                        v.replace('\\', "\\\\").replace('"', "\\\"")
                    )
                })
                .collect();
            json.push_str(&attrs.join(","));
            json.push('}');
        }

        json.push('}');
        json
    }

    /// Format for human reading
    pub fn to_human(&self) -> String {
        let timestamp = self
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut output = format!(
            "{} {} [{}] {}",
            timestamp, self.level, self.source, self.message
        );

        if let Some(ref trace_id) = self.trace_id {
            output.push_str(&format!(" trace_id={}", trace_id));
        }

        for (k, v) in &self.attributes {
            output.push_str(&format!(" {}={}", k, v));
        }

        output
    }
}

/// Structured logger
pub struct StructuredLogger {
    config: LogConfig,
    entries: Mutex<Vec<LogEntry>>,
    /// Current trace context
    trace_context: Mutex<Option<(String, String)>>,
}

impl StructuredLogger {
    /// Create a new structured logger
    pub fn new(config: LogConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(Vec::new()),
            trace_context: Mutex::new(None),
        }
    }

    /// Set the current trace context
    pub fn set_context(&self, trace_id: &str, span_id: &str) {
        let mut ctx = self.trace_context.lock().unwrap();
        *ctx = Some((trace_id.to_string(), span_id.to_string()));
    }

    /// Clear the current trace context
    pub fn clear_context(&self) {
        let mut ctx = self.trace_context.lock().unwrap();
        *ctx = None;
    }

    /// Log at trace level
    pub fn trace(&self, message: &str, attrs: &[(&str, &str)]) {
        self.log(LogLevel::Trace, message, attrs);
    }

    /// Log at debug level
    pub fn debug(&self, message: &str, attrs: &[(&str, &str)]) {
        self.log(LogLevel::Debug, message, attrs);
    }

    /// Log at info level
    pub fn info(&self, message: &str, attrs: &[(&str, &str)]) {
        self.log(LogLevel::Info, message, attrs);
    }

    /// Log at warn level
    pub fn warn(&self, message: &str, attrs: &[(&str, &str)]) {
        self.log(LogLevel::Warn, message, attrs);
    }

    /// Log at error level
    pub fn error(&self, message: &str, attrs: &[(&str, &str)]) {
        self.log(LogLevel::Error, message, attrs);
    }

    /// Log stdout from a process
    pub fn log_stdout(&self, output: &str, step_name: &str) {
        if !self.config.include_stdout {
            return;
        }

        for line in output.lines() {
            let mut entry = LogEntry::new(LogLevel::Debug, line).with_source("stdout");
            entry
                .attributes
                .insert("step".to_string(), step_name.to_string());
            self.record_entry(entry);
        }
    }

    /// Log stderr from a process
    pub fn log_stderr(&self, output: &str, step_name: &str) {
        if !self.config.include_stderr {
            return;
        }

        for line in output.lines() {
            let mut entry = LogEntry::new(LogLevel::Warn, line).with_source("stderr");
            entry
                .attributes
                .insert("step".to_string(), step_name.to_string());
            self.record_entry(entry);
        }
    }

    fn log(&self, level: LogLevel, message: &str, attrs: &[(&str, &str)]) {
        if !self.config.enabled || level < self.config.level {
            return;
        }

        let mut entry = LogEntry::new(level, message);

        // Add trace context if available
        if let Some((ref trace_id, ref span_id)) = *self.trace_context.lock().unwrap() {
            entry.trace_id = Some(trace_id.clone());
            entry.span_id = Some(span_id.clone());
        }

        // Add attributes
        for (k, v) in attrs {
            entry.attributes.insert(k.to_string(), v.to_string());
        }

        // Output to tracing if configured
        if self.config.output_to_tracing {
            match level {
                LogLevel::Trace => tracing::trace!("{}", message),
                LogLevel::Debug => tracing::debug!("{}", message),
                LogLevel::Info => tracing::info!("{}", message),
                LogLevel::Warn => tracing::warn!("{}", message),
                LogLevel::Error => tracing::error!("{}", message),
            }
        }

        self.record_entry(entry);
    }

    fn record_entry(&self, mut entry: LogEntry) {
        // Add trace context if available
        if entry.trace_id.is_none() {
            if let Some((ref trace_id, ref span_id)) = *self.trace_context.lock().unwrap() {
                entry.trace_id = Some(trace_id.clone());
                entry.span_id = Some(span_id.clone());
            }
        }

        if self.config.in_memory {
            let mut entries = self.entries.lock().unwrap();
            if entries.len() >= self.config.max_entries {
                entries.remove(0);
            }
            entries.push(entry);
        }
    }

    /// Get all collected entries
    pub fn get_entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Get entries filtered by level
    pub fn get_entries_by_level(&self, min_level: LogLevel) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.level >= min_level)
            .cloned()
            .collect()
    }

    /// Get entries filtered by source
    pub fn get_entries_by_source(&self, source: &str) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.source == source)
            .cloned()
            .collect()
    }

    /// Clear all entries
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// Check if any entry contains the given substring
    pub fn contains(&self, substring: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.message.contains(substring))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_levels() {
        assert!(LogLevel::Error > LogLevel::Warn);
        assert!(LogLevel::Warn > LogLevel::Info);
        assert!(LogLevel::Info > LogLevel::Debug);
        assert!(LogLevel::Debug > LogLevel::Trace);
    }

    #[test]
    fn test_log_entry_creation() {
        let entry = LogEntry::new(LogLevel::Info, "test message")
            .with_trace_id("trace123")
            .with_span_id("span456")
            .with_attribute("key", "value");

        assert_eq!(entry.message, "test message");
        assert_eq!(entry.trace_id, Some("trace123".to_string()));
        assert_eq!(entry.span_id, Some("span456".to_string()));
        assert_eq!(entry.attributes.get("key"), Some(&"value".to_string()));
    }

    #[test]
    fn test_log_entry_json() {
        let entry = LogEntry::new(LogLevel::Info, "test")
            .with_trace_id("abc123")
            .with_attribute("key", "value");

        let json = entry.to_json();
        assert!(json.contains("\"level\":\"INFO\""));
        assert!(json.contains("\"message\":\"test\""));
        assert!(json.contains("\"trace_id\":\"abc123\""));
    }

    #[test]
    fn test_structured_logger() {
        let logger = StructuredLogger::new(LogConfig::in_memory());

        logger.info("test message", &[("key", "value")]);
        logger.debug("debug message", &[]);
        logger.error("error message", &[]);

        let entries = logger.get_entries();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_logger_level_filter() {
        let mut config = LogConfig::in_memory();
        config.level = LogLevel::Warn;

        let logger = StructuredLogger::new(config);

        logger.debug("should not appear", &[]);
        logger.info("should not appear", &[]);
        logger.warn("should appear", &[]);
        logger.error("should appear", &[]);

        let entries = logger.get_entries();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_logger_trace_context() {
        let logger = StructuredLogger::new(LogConfig::in_memory());

        logger.set_context("trace123", "span456");
        logger.info("message with context", &[]);

        let entries = logger.get_entries();
        assert_eq!(entries[0].trace_id, Some("trace123".to_string()));
        assert_eq!(entries[0].span_id, Some("span456".to_string()));
    }

    #[test]
    fn test_logger_stdout_stderr() {
        let logger = StructuredLogger::new(LogConfig::in_memory());

        logger.log_stdout("line 1\nline 2", "step1");
        logger.log_stderr("error line", "step1");

        let entries = logger.get_entries();
        assert_eq!(entries.len(), 3);

        let stdout_entries = logger.get_entries_by_source("stdout");
        assert_eq!(stdout_entries.len(), 2);

        let stderr_entries = logger.get_entries_by_source("stderr");
        assert_eq!(stderr_entries.len(), 1);
    }

    #[test]
    fn test_logger_contains() {
        let logger = StructuredLogger::new(LogConfig::in_memory());

        logger.info("hello world", &[]);
        logger.info("foo bar", &[]);

        assert!(logger.contains("world"));
        assert!(logger.contains("foo"));
        assert!(!logger.contains("missing"));
    }
}
