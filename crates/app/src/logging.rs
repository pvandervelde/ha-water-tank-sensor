// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions for setting up the logging system

use core::cell::RefCell;
use core::fmt;
use core::fmt::Write;
use core::str::FromStr;

use critical_section::Mutex;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::TcpClient;
use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_time::Duration;
use esp_hal::time::now;
use heapless::String;
use heapless::Vec;
use log::error;
use log::Level;
use log::LevelFilter;
use log::Log;
use log::Metadata;
use log::Record;

use esp_println::println;
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::RequestBuilder;
use serde::Serialize;
use thiserror::Error;

use crate::device_meta::DEVICE_LOCATION;
use crate::device_meta::MAX_DEVICE_NAME_LENGTH;
use crate::wifi::DEFAULT_TCP_TIMEOUT_IN_MILLISECONDS;

// Constants for buffer sizes
const MAX_STORED_LOGS: usize = 100;
const MAX_LOG_LENGTH: usize = 256;

// HTTP specific constants
const LOGGING_URL: &str = env!("LOGGING_URL");
const LOGGING_URL_SUB_PATH: &str = "/api/v1/logs";

// Create a static mutex-protected log buffer
static LOG_BUFFER: Mutex<RefCell<heapless::Deque<LogEntry, MAX_STORED_LOGS>>> =
    Mutex::new(RefCell::new(heapless::Deque::new()));

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to push log to the buffer")]
    PushLogToBuffer,

    #[error("Failed to send logs to the remote system")]
    SendLogs,

    #[error("Failed to set the global logger. No logs will be provided.")]
    SetLogger,
}

// Log entry structure
#[derive(Clone, Serialize)]
struct LogEntry {
    device_id: String<MAX_DEVICE_NAME_LENGTH>,
    level: String<32>,
    message: String<MAX_LOG_LENGTH>,
    boot_count: u32,
    timestamp: u64, // Simple timestamp (milliseconds since boot)
}
// HTTP Logger implementation
pub struct HttpLogger {
    boot_count: core::sync::atomic::AtomicU32,
}

impl HttpLogger {
    pub const fn new() -> Self {
        Self {
            boot_count: core::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn set_boot_count(&self, count: u32) {
        self.boot_count
            .store(count, core::sync::atomic::Ordering::Relaxed);
    }

    // Store a log entry in the buffer
    fn store_log(&self, record: &Record) -> Result<(), Error> {
        let level = record.level();

        let location = match String::try_from(DEVICE_LOCATION) {
            Ok(l) => l,
            Err(_) => String::new(),
        };

        let level_as_str = match String::try_from(level.as_str()) {
            Ok(l) => l,
            Err(_) => String::new(),
        };

        // Format the log message
        let mut message = String::new();
        let _ = write!(message, "{}", record.args());

        // Create the log entry
        let entry = LogEntry {
            device_id: location,
            boot_count: self.boot_count.load(core::sync::atomic::Ordering::Relaxed),
            level: level_as_str,
            message,
            timestamp: now().ticks(),
        };

        // Get mutable access to the buffer through the mutex
        critical_section::with(|cs| {
            let mut buffer = LOG_BUFFER.borrow_ref_mut(cs);
            // Try to store the log, removing oldest entry if full
            if buffer.is_full() {
                let _ = buffer.pop_front();
            }

            if buffer.push_back(entry).is_err() {
                return Err(Error::PushLogToBuffer);
            }

            Ok(())
        })
    }
}

// Implement the Log trait for HttpLogger
impl Log for HttpLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        /// Log level from environment
        const LEVEL: Option<&'static str> = option_env!("ESP_LOG");

        let max_level = LEVEL
            .map(|level| Level::from_str(level).unwrap_or(Level::Info))
            .unwrap_or(Level::Info);

        metadata.level() <= max_level
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            // Store the log entry
            let _ = self.store_log(record);

            log_to_console(record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {
        // The Log trait's flush method doesn't have access to the async context
        // Actual flushing happens in the logger task
    }
}

fn log_to_console(level: Level, target: &str, args: &fmt::Arguments) {
    /// Modifier for restoring normal text style
    const RESET: &str = "\u{001B}[0m";
    /// Modifier for setting gray text
    const GRAY: &str = "\u{001B}[2m";
    /// Modifier for setting red text
    const RED: &str = "\u{001B}[31m";
    /// Modifier for setting green text
    const GREEN: &str = "\u{001B}[32m";
    /// Modifier for setting yellow text
    const YELLOW: &str = "\u{001B}[33m";
    /// Modifier for setting blue text
    const BLUE: &str = "\u{001B}[34m";
    /// Modifier for setting cyan text
    const CYAN: &str = "\u{001B}[35m";

    let color = match level {
        Level::Error => RED,
        Level::Warn => YELLOW,
        Level::Info => GREEN,
        Level::Debug => BLUE,
        Level::Trace => CYAN,
    };

    // Print to console with colors
    println!(
        "{}{:>5} {}{}{}{}{} {}",
        color, level, RESET, GRAY, target, GRAY, RESET, args
    );
}

pub async fn send_logs_to_server(stack: Stack<'static>) -> Result<(), Error> {
    let mut temp_log_buffer: Vec<LogEntry, MAX_STORED_LOGS> = Vec::new();

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::logger_task",
        &format_args!("Sending logs to server ..."),
    );

    // Take logs from the main buffer if our temp buffer is empty
    if temp_log_buffer.is_empty() {
        log_to_console(
            Level::Debug,
            "tank_sensor_level_embedded::logging::logger_task",
            &format_args!("Attempting to push logs to buffer ..."),
        );
        critical_section::with(|cs| {
            let mut buffer = LOG_BUFFER.borrow_ref_mut(cs);
            while !buffer.is_empty() && !temp_log_buffer.is_full() {
                if let Some(entry) = buffer.pop_front() {
                    let _ = temp_log_buffer.push(entry);
                }
            }
        });
    }

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::logger_task",
        &format_args!("Sending logs to server ..."),
    );
    loop {
        // If we have logs to send and enough time has passed
        if !temp_log_buffer.is_empty() {
            // Try to send logs
            log_to_console(
                Level::Debug,
                "tank_sensor_level_embedded::logging::logger_task",
                &format_args!("Sending logs to server ..."),
            );
            match transmit_logs(&temp_log_buffer, stack, LOGGING_URL).await {
                Ok(()) => {
                    // Success - clear sent logs
                    temp_log_buffer.clear();
                    log_to_console(
                        Level::Info,
                        "tank_sensor_level_embedded::logging::logger_task",
                        &format_args!("Logs send to server successfully"),
                    );
                    break;
                }
                Err(e) => {
                    log_to_console(
                        Level::Error,
                        "tank_sensor_level_embedded::logging::logger_task",
                        &format_args!("Failed to send logs to the server. Error was {e:?}"),
                    );
                }
            }
        } else if temp_log_buffer.is_empty() {
            // No logs to send, signal idle
            log_to_console(
                Level::Debug,
                "tank_sensor_level_embedded::logging::logger_task",
                &format_args!("No logs to send ..."),
            );

            break;
        }
    }

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::logger_task",
        &format_args!("Sent all logs to server"),
    );

    Ok(())
}

/// Setup logging
///
/// To change the log level change the `env` section in `.cargo/config.toml`
/// or remove it and set the environment variable `ESP_LOG` manually before
/// running `cargo run`.
///
/// This requires a clean rebuild because of
/// <https://github.com/rust-lang/cargo/issues/10358>
pub fn setup_logger(boot_count: u32) -> Result<(), Error> {
    // Initialize the static buffer
    static LOGGER: HttpLogger = HttpLogger::new();

    // Initialize the logger with the boot count
    LOGGER.set_boot_count(boot_count);

    // Set the logger
    let logger_set_result = log::set_logger(&LOGGER);
    if logger_set_result.is_err() {
        return Err(Error::SetLogger);
    }

    /// Log level
    const LEVEL: Option<&'static str> = option_env!("ESP_LOG");
    if let Some(level) = LEVEL {
        let level = LevelFilter::from_str(level).unwrap_or(LevelFilter::Off);

        log::set_max_level(level);
    }

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::setup()",
        &format_args!("Logger is ready"),
    );

    Ok(())
}

async fn transmit_logs(logs: &[LogEntry], stack: Stack<'_>, url: &str) -> Result<(), Error> {
    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let mut tcp_client = TcpClient::new(stack, &tcp_client_state);
    tcp_client.set_timeout(Some(Duration::from_millis(DEFAULT_TCP_TIMEOUT_IN_MILLISECONDS)));

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::transmit_logs()",
        &format_args!("Creating HTTP client ..."),
    );
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    let mut rx_buf = [0; 4096];

    // Convert logs to JSON using serde_json_core (heapless)
    let mut json_buffer = [0u8; 2048];

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::transmit_logs()",
        &format_args!("Selecting logs to send ..."),
    );
    for chunk in logs.chunks(10) {
        match serde_json_core::to_slice(chunk, &mut json_buffer) {
            Ok(size) => {
                let resource_result = client.resource(url).await;
                let mut resource = match resource_result {
                    Ok(r) => r,
                    Err(_) => {
                        log_to_console(
                            Level::Error,
                            "tank_sensor_level_embedded::logging::transmit_logs()",
                            &format_args!("Failed to create request ..."),
                        );
                        return Err(Error::SendLogs);
                    }
                };

                let response = resource
                    .post(LOGGING_URL_SUB_PATH)
                    .content_type(ContentType::ApplicationJson)
                    .body(&json_buffer[..size]);

                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::transmit_logs()",
                    &format_args!("Sending log POST request ..."),
                );
                let response = response.send(&mut rx_buf).await;

                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::transmit_logs()",
                    &format_args!("Processing log POST response ..."),
                );
                match response {
                    Ok(r) => {
                        if r.status.is_successful() {
                            log_to_console(
                                Level::Debug,
                                "tank_sensor_level_embedded::logging::transmit_logs()",
                                &format_args!("Sent logs. Status code: {:?}", r.status),
                            );
                        } else {
                            log_to_console(
                                Level::Error,
                                "tank_sensor_level_embedded::logging::transmit_logs()",
                                &format_args!("Failed to send logs: Status code {:?}", r.status),
                            );
                        }
                    }
                    Err(e) => {
                        log_to_console(
                            Level::Error,
                            "tank_sensor_level_embedded::logging::transmit_logs()",
                            &format_args!("Failed to send logs: error {:?}", e),
                        );
                    }
                }
            }
            Err(e) => {
                log_to_console(
                    Level::Error,
                    "tank_sensor_level_embedded::logging::transmit_logs()",
                    &format_args!("Failed to send logs: error {:?}", e),
                );
            }
        }
    }

    Ok(())
}
