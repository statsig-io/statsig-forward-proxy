//! A `log` implementation that redacts Statsig SDK keys from every record.
//!
//! Rocket logs the full request URI when no route matches (`No matching
//! routes for GET /v1/...`), and SDK keys travel in the path and query of
//! those requests. Rocket only installs its own logger when none is set, so
//! installing this one before launch routes every Rocket record through
//! [`redact_sdk_keys`] while keeping Rocket's output shape.

use std::io::Write;

use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

use crate::servers::sdk_key_normalizer::redact_sdk_keys;

/// Destination for formatted log lines. Abstracted so tests can capture output.
pub trait LogSink: Send + Sync + 'static {
    fn write_line(&self, line: &str);
}

/// Writes each line to stdout, falling back to stderr like Rocket's logger does.
pub struct StdoutSink;

impl LogSink for StdoutSink {
    fn write_line(&self, line: &str) {
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = writeln!(stdout, "{line}") {
            let _ = writeln!(std::io::stderr(), "{error}");
        }
    }
}

pub struct SdkKeyRedactingLogger<S: LogSink> {
    sink: S,
}

impl<S: LogSink> SdkKeyRedactingLogger<S> {
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S: LogSink> Log for SdkKeyRedactingLogger<S> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        // Like Rocket's logger, hide transport-level chatter unless Rocket's `debug`
        // log level is active. Rocket maps that level to `LevelFilter::Trace`.
        let from = |prefix: &str| record.module_path().is_some_and(|m| m.starts_with(prefix));
        if log::max_level() < LevelFilter::from(rocket::config::LogLevel::Debug)
            && (from("hyper") || from("rustls") || from("r2d2"))
        {
            return;
        }

        let raw_message = record.args().to_string();
        let message = redact_sdk_keys(&raw_message);
        // Rocket marks indented continuation lines with a trailing `_` in the target.
        let indented = record.target().ends_with('_');
        let line = match (record.level(), indented) {
            (_, true) => format!("   >> {message}"),
            (log::Level::Error, false) => format!("Error: {message}"),
            (log::Level::Warn, false) => format!("Warning: {message}"),
            (log::Level::Debug, false) => match (record.file(), record.line()) {
                (Some(file), Some(line)) => format!("--> {file}:{line}\n\t{message}"),
                _ => message.into_owned(),
            },
            _ => message.into_owned(),
        };
        self.sink.write_line(&line);
    }

    fn flush(&self) {}
}

/// The `log` level Rocket would have used, read from the same figment Rocket
/// reads (`Rocket.toml` plus `ROCKET_*` environment variables).
pub fn rocket_log_level_filter() -> LevelFilter {
    rocket::Config::try_from(rocket::Config::figment())
        .map(|config| config.log_level.into())
        .unwrap_or(LevelFilter::Warn)
}

/// Installs the redacting logger as the global `log` logger. Must run before
/// Rocket launches, otherwise Rocket installs its own unredacted logger first.
pub fn install_sdk_key_redacting_logger(max_level: LevelFilter) -> Result<(), SetLoggerError> {
    log::set_boxed_logger(Box::new(SdkKeyRedactingLogger::new(StdoutSink)))?;
    log::set_max_level(max_level);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LogSink, SdkKeyRedactingLogger};
    use log::{Level, Log, Record};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CapturingSink(Arc<Mutex<Vec<String>>>);

    impl LogSink for CapturingSink {
        fn write_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_owned());
        }
    }

    fn log_line(level: Level, target: &str, message: &str) -> String {
        let sink = CapturingSink::default();
        let logger = SdkKeyRedactingLogger::new(sink.clone());
        logger.log(
            &Record::builder()
                .level(level)
                .target(target)
                .module_path(Some("rocket::server"))
                .args(format_args!("{message}"))
                .build(),
        );
        let lines = sink.0.lock().unwrap();
        assert_eq!(lines.len(), 1);
        lines[0].clone()
    }

    #[test]
    fn redacts_sdk_keys_and_keeps_rocket_error_shape() {
        assert_eq!(
            log_line(
                Level::Error,
                "rocket::server::_",
                "No matching routes for HEAD /v1/download_config_specs/client-abcdefghijklmnopqrstuvwxyz.json?token=secret-abcdefghijklmnopqrstuvwxyz."
            ),
            "   >> No matching routes for HEAD /v1/download_config_specs/client-abcdefghijklm***.json?token=secret-abcdefghijklm***."
        );
        assert_eq!(
            log_line(Level::Error, "rocket::server", "boom"),
            "Error: boom"
        );
        assert_eq!(
            log_line(Level::Warn, "rocket::server", "careful"),
            "Warning: careful"
        );
        assert_eq!(log_line(Level::Info, "rocket::server", "hello"), "hello");
    }
}
