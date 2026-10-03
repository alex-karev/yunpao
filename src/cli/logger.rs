use cliclack::{Theme, log as clilog};
use log::{Level, Log, Metadata, Record};

pub struct CliclackLogger;

impl Log for CliclackLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let msg = format!("{}", record.args());
        match record.level() {
            Level::Debug => clilog::step(msg).ok(),
            Level::Info => clilog::info(msg).ok(),
            Level::Warn => clilog::warning(msg).ok(),
            Level::Error => clilog::error(msg).ok(),
            Level::Trace => clilog::remark(msg).ok(),
        };
    }

    fn flush(&self) {}
}

pub struct CliclackTheme;

impl Theme for CliclackTheme {
    fn format_intro(&self, title: &str) -> String {
        format!("=== {title} ===\n")
    }
    fn format_outro(&self, message: &str) -> String {
        format!("=== {message} ===\n")
    }
    fn format_outro_cancel(&self, message: &str) -> String {
        format!("=== {message} ===\n")
    }
    fn format_log_with_spacing(&self, text: &str, symbol: &str, spacing: bool) -> String {
        format!("{symbol} {text}\n{}", if spacing { "\n" } else { "" })
    }
    fn format_log(&self, text: &str, symbol: &str) -> String {
        format!("{symbol} {text}\n")
    }
    fn remark_symbol(&self) -> String {
        String::from("-")
    }
}
