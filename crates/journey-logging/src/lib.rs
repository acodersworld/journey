use std::io::{self, Write};

use log::{LevelFilter, Log, Metadata, Record};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingSettings {
    pub level: String,
    /// Accepted for compatibility with earlier configuration files; ignored.
    pub queue_capacity: usize,
    /// Accepted for compatibility with earlier configuration files; ignored.
    pub slow_enqueue_warning_ms: u64,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            queue_capacity: 65_536,
            slow_enqueue_warning_ms: 100,
        }
    }
}

impl LoggingSettings {
    fn level_filter(&self) -> io::Result<LevelFilter> {
        match self.level.as_str() {
            "off" => Ok(LevelFilter::Off),
            "error" => Ok(LevelFilter::Error),
            "warn" => Ok(LevelFilter::Warn),
            "info" => Ok(LevelFilter::Info),
            "debug" => Ok(LevelFilter::Debug),
            "trace" => Ok(LevelFilter::Trace),
            level => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("logging.level must be off, error, warn, info, debug, or trace (got {level:?})"),
            )),
        }
    }

    fn validate(&self) -> io::Result<LevelFilter> {
        self.level_filter()
    }
}

struct DirectLogger {
    identity: String,
    filter: LevelFilter,
}

impl Log for DirectLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.filter
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let timestamp = jiff::Timestamp::now();
        let message = record.args().to_string();
        let mut message = message.splitn(2, char::is_whitespace);
        let event = message.next().filter(|event| !event.is_empty()).unwrap_or("message");
        let fields = message.next().unwrap_or("").trim();
        let formatted_fields = if fields.is_empty() {
            String::new()
        } else if fields.split_whitespace().all(|field| field.contains('=')) {
            format!(" {}", escape(fields))
        } else {
            format!(" message={:?}", escape(fields))
        };
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{} {} service={} event={}{}",
            timestamp,
            record.level(),
            escape(&self.identity),
            escape(event),
            formatted_fields,
        );
    }

    fn flush(&self) {
        let _ = io::stderr().lock().flush();
    }
}

/// Installs a logger that writes each record directly to stderr.
pub fn initialize(identity: impl Into<String>, settings: &LoggingSettings) -> io::Result<()> {
    let filter = settings.validate()?;
    let logger = DirectLogger { identity: identity.into(), filter };
    log::set_boxed_logger(Box::new(logger)).map_err(|error| {
        io::Error::new(io::ErrorKind::AlreadyExists, format!("could not initialize logger: {error}"))
    })?;
    log::set_max_level(filter);
    Ok(())
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{{{:x}}}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    #[test]
    fn logging_defaults_and_level_validation_are_stable() {
        let settings = LoggingSettings::default();
        assert_eq!(settings.level, "info");
        assert_eq!(settings.queue_capacity, 65_536);
        assert_eq!(settings.slow_enqueue_warning_ms, 100);
        assert_eq!(settings.validate().unwrap(), LevelFilter::Info);

        for (name, expected) in [
            ("off", LevelFilter::Off),
            ("error", LevelFilter::Error),
            ("warn", LevelFilter::Warn),
            ("info", LevelFilter::Info),
            ("debug", LevelFilter::Debug),
            ("trace", LevelFilter::Trace),
        ] {
            let settings = LoggingSettings { level: name.to_owned(), ..LoggingSettings::default() };
            assert_eq!(settings.validate().unwrap(), expected);
        }
    }

    #[test]
    fn logging_settings_reject_unknown_levels() {
        let settings = LoggingSettings { level: "verbose".to_owned(), ..LoggingSettings::default() };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn disabled_levels_are_filtered_before_output() {
        let logger = DirectLogger { identity: "test".to_owned(), filter: LevelFilter::Info };
        let debug = Metadata::builder().level(Level::Debug).target("test").build();
        let info = Metadata::builder().level(Level::Info).target("test").build();
        assert!(!logger.enabled(&debug));
        assert!(logger.enabled(&info));
    }

    #[test]
    fn control_characters_are_escaped_for_single_line_output() {
        assert_eq!(escape("first\nsecond\t\u{1b}"), "first\\nsecond\\t\\x1b");
    }

}
