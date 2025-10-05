use std::fs::OpenOptions;
use std::io::{ self, Write };
use std::sync::Mutex;
use chrono::Utc;
//  order of log (Debug < Info < Warning < Error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
}

impl LogLevel {
    fn as_str(&self) -> &str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warning => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Debug => "DEBUG",
        }
    }
}

pub struct Logger {
    file_path: String,
    file_mutex: Mutex<()>,
    enabled: bool,
    min_level: LogLevel,
}

impl Logger {
    pub fn new(file_path: &str, enabled: bool) -> Result<Self, io::Error> {
        Self::new_with_level(file_path, enabled, LogLevel::Debug)
    }

    pub fn new_with_level(
        file_path: &str,
        enabled: bool,
        min_level: LogLevel
    ) -> Result<Self, io::Error> {
        if enabled {
            // ensure file exists
            OpenOptions::new().create(true).append(true).open(file_path)?;
        }
        Ok(Logger {
            file_path: file_path.to_string(),
            file_mutex: Mutex::new(()),
            enabled,
            min_level,
        })
    }

    // Convenience constructors for common configurations
    pub fn new_production(file_path: &str) -> Result<Self, io::Error> {
        Self::new_with_level(file_path, true, LogLevel::Info)
    }

    pub fn new_development(file_path: &str) -> Result<Self, io::Error> {
        Self::new_with_level(file_path, true, LogLevel::Debug)
    }

    fn should_log(&self, level: LogLevel) -> bool {
        self.enabled && level >= self.min_level
    }

    pub fn log(&self, level: LogLevel, message: &str) -> Result<(), io::Error> {
        if !self.should_log(level) {
            return Ok(());
        }

        let _guard = self.file_mutex.lock().unwrap();

        let timestamp = Utc::now();
        let formatted_message = format!(
            "[{}] [{}] {}\n",
            timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
            level.as_str(),
            message
        );

        let mut file = OpenOptions::new().create(true).append(true).open(&self.file_path)?;
        file.write_all(formatted_message.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    pub fn log_fmt(&self, level: LogLevel, args: std::fmt::Arguments) -> Result<(), io::Error> {
        if !self.should_log(level) {
            return Ok(());
        }
        self.log(level, &format!("{}", args))
    }

    pub fn info(&self, message: &str) -> Result<(), io::Error> {
        self.log(LogLevel::Info, message)
    }
    pub fn warn(&self, message: &str) -> Result<(), io::Error> {
        self.log(LogLevel::Warning, message)
    }
    pub fn error(&self, message: &str) -> Result<(), io::Error> {
        self.log(LogLevel::Error, message)
    }
    pub fn debug(&self, message: &str) -> Result<(), io::Error> {
        self.log(LogLevel::Debug, message)
    }

    pub fn info_fmt(&self, args: std::fmt::Arguments) -> Result<(), io::Error> {
        self.log_fmt(LogLevel::Info, args)
    }
    pub fn warn_fmt(&self, args: std::fmt::Arguments) -> Result<(), io::Error> {
        self.log_fmt(LogLevel::Warning, args)
    }
    pub fn error_fmt(&self, args: std::fmt::Arguments) -> Result<(), io::Error> {
        self.log_fmt(LogLevel::Error, args)
    }
    pub fn debug_fmt(&self, args: std::fmt::Arguments) -> Result<(), io::Error> {
        self.log_fmt(LogLevel::Debug, args)
    }

    pub fn clear(&self) -> Result<(), io::Error> {
        if !self.enabled {
            return Ok(());
        }
        let _guard = self.file_mutex.lock().unwrap();
        std::fs::write(&self.file_path, "")?;
        Ok(())
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    pub fn min_level(&self) -> LogLevel {
        self.min_level
    }
    pub fn set_min_level(&mut self, level: LogLevel) {
        self.min_level = level;
    }
}

#[macro_export]
macro_rules! log_info {
    (
        $logger:expr,
        $($arg:tt)*
    ) => {
        $logger.info_fmt(format_args!($($arg)*))
    };
}
#[macro_export]
macro_rules! log_warn {
    (
        $logger:expr,
        $($arg:tt)*
    ) => {
        $logger.warn_fmt(format_args!($($arg)*))
    };
}
#[macro_export]
macro_rules! log_error {
    (
        $logger:expr,
        $($arg:tt)*
    ) => {
        $logger.error_fmt(format_args!($($arg)*))
    };
}
#[macro_export]
macro_rules! log_debug {
    (
        $logger:expr,
        $($arg:tt)*
    ) => {
        $logger.debug_fmt(format_args!($($arg)*))
    };
}
