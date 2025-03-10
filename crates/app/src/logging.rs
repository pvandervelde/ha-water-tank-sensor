// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions for setting up the logging system

use core::cell::RefCell;
use core::fmt;
use core::fmt::Write;
use core::str::FromStr;

#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::TcpClient;
use embassy_net::tcp::client::TcpClientState;
use embassy_net::IpEndpoint;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::channel::Receiver;
use embassy_sync::channel::Sender;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use embassy_time::Timer;
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

// Constants for buffer sizes
const MAX_STORED_LOGS: usize = 100;
const MAX_LOG_LENGTH: usize = 256;
const HTTP_BUFFER_SIZE: usize = 2048;

// HTTP specific constants
const LOGGING_URL: &str = env!("LOGGING_URL");
const LOGGING_URL_SUB_PATH: &str = "/api/v1/logs";

// Create static channels for logger communication
static LOGGER_SHUT_DOWN_REQUESTED_CHANNEL: Signal<CriticalSectionRawMutex, bool> = Signal::new();
static LOGGER_SHUT_DOWN_COMPLETE_CHANNEL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

static LOGGER: LazyLock<HttpLogger> = LazyLock::new(|| HttpLogger::new());

// Create a static mutex-protected log buffer
static LOG_BUFFER: Mutex<RefCell<heapless::Deque<LogEntry, MAX_STORED_LOGS>>> =
    Mutex::new(RefCell::new(heapless::Deque::new()));

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to push log to the buffer")]
    FailedToPushLogToBuffer,

    #[error("Failed to serialize the logs.")]
    FailedToSerializeLogs,

    #[error("Failed to set the global logger. No logs will be provided.")]
    FailedToSetLogger,

    #[error("The POST request to send logs resulted in a non-success response code.")]
    NonSuccessResponseCode,

    #[error("The log sending request failed")]
    RequestFailed,
}

// Log entry structure
#[derive(Clone, Serialize)]
struct LogEntry {
    level: Level,
    message: String<MAX_LOG_LENGTH>,
    timestamp: u64, // Simple timestamp (milliseconds since boot)
}

// HTTP Logger implementation
pub struct HttpLogger {}

impl HttpLogger {
    pub fn new() -> Self {
        Self {}
    }

    // Store a log entry in the buffer
    fn store_log(&self, record: &Record) -> Result<(), Error> {
        let level = record.level();

        // Format the log message
        let mut message = String::new();
        let _ = write!(message, "{}", record.args());

        // Create the log entry
        let entry = LogEntry {
            level,
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
                return Err(Error::FailedToPushLogToBuffer);
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

// Increase retry interval with exponential backoff
fn increase_retry_interval(interval: &mut u64) {
    const MAX_RETRY_INTERVAL: u64 = 600000; // 10 minutes
    *interval = (*interval * 2).min(MAX_RETRY_INTERVAL);
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
        "{}{:>5} {}{}{}{}]{} {}",
        color, level, RESET, GRAY, target, GRAY, RESET, args
    );
}

#[embassy_executor::task]
pub async fn logger_task(
    net_stack_provider: Receiver<'static, NoopRawMutex, Stack<'static>, 1>,
    shut_down_requested_signal: &'static Signal<CriticalSectionRawMutex, bool>,
    shut_down_complete_signal: &'static Signal<CriticalSectionRawMutex, bool>,
) {
    let mut shutdown_requested = false;
    let mut temp_log_buffer: Vec<LogEntry, MAX_STORED_LOGS> = Vec::new();
    let mut retry_interval = 10000; // 10 seconds
    let mut last_send_attempt = 0;

    let mut stack: Option<Stack> = None;

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::logger_task",
        &format_args!("Starting logging sending loop ..."),
    );
    loop {
        shutdown_requested = shut_down_requested_signal.signaled();

        if stack.is_none() {
            let potential_stack = net_stack_provider.try_receive();
            if let Ok(net_stack) = potential_stack {
                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::logger_task",
                    &format_args!("Network stack obtained. Starting log sending ..."),
                );
                stack = Some(net_stack);
            } else {
                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::logger_task",
                    &format_args!("No network stack found. Unable to send logs ..."),
                );
            }
        } else {
            let current_time = embassy_time::Instant::now().as_millis();
            let time_since_last_attempt = current_time.saturating_sub(last_send_attempt);

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

            // If we have logs to send and enough time has passed
            if !temp_log_buffer.is_empty() && time_since_last_attempt >= retry_interval {
                last_send_attempt = current_time;

                // Try to send logs
                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::logger_task",
                    &format_args!("Sending logs to server ..."),
                );
                match send_logs_to_server(&temp_log_buffer, stack.unwrap(), LOGGING_URL).await {
                    Ok(()) => {
                        // Success - clear sent logs
                        temp_log_buffer.clear();
                        retry_interval = 10000; // Reset retry interval
                    }
                    Err(_) => {
                        // Failed - increase retry interval
                        increase_retry_interval(&mut retry_interval);
                    }
                }
            } else if temp_log_buffer.is_empty() {
                // No logs to send, signal idle
                log_to_console(
                    Level::Debug,
                    "tank_sensor_level_embedded::logging::logger_task",
                    &format_args!("No logs to send ..."),
                );

                // If shutdown is requested and we've sent all logs, complete shutdown
                if shutdown_requested {
                    break;
                }
            }
        }

        // If shutdown is requested but network is not available, just complete shutdown
        if shutdown_requested {
            log_to_console(
                Level::Debug,
                "tank_sensor_level_embedded::logging::logger_task",
                &format_args!("Shutting down logger loop ..."),
            );
            break;
        }

        // Wait a bit before checking again
        Timer::after(Duration::from_millis(100)).await;
    }

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::logger_task",
        &format_args!("Shutting down logger task"),
    );

    shut_down_complete_signal.signal(true);
}

// Function to send logs via HTTP
async fn send_logs_to_server(logs: &[LogEntry], stack: Stack<'_>, url: &str) -> Result<(), Error> {
    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::send_logs_to_server()",
        &format_args!("Creating HTTP client ..."),
    );
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    // Convert logs to JSON using serde_json_core (heapless)
    let mut json_buffer = [0u8; 2048];
    let logs_slice = if logs.len() > 10 { &logs[0..10] } else { logs }; // Limit batch size

    log_to_console(
        Level::Debug,
        "tank_sensor_level_embedded::logging::send_logs_to_server()",
        &format_args!("Selecting logs to send ..."),
    );
    let mut rx_buf = [0; 4096];
    match serde_json_core::to_slice(logs_slice, &mut json_buffer) {
        Ok(size) => {
            let mut resource = client.resource(url).await.unwrap();
            let response = resource
                .post(LOGGING_URL_SUB_PATH)
                .content_type(ContentType::ApplicationJson)
                .body(&json_buffer[..size]);

            log_to_console(
                Level::Debug,
                "tank_sensor_level_embedded::logging::send_logs_to_server()",
                &format_args!("Sending log POST request ..."),
            );
            let response = response.send(&mut rx_buf).await;

            log_to_console(
                Level::Debug,
                "tank_sensor_level_embedded::logging::send_logs_to_server()",
                &format_args!("Processing log POST response ..."),
            );
            match response {
                Ok(r) => {
                    if r.status.is_successful() {
                        log_to_console(
                            Level::Debug,
                            "tank_sensor_level_embedded::logging::send_logs_to_server()",
                            &format_args!("Sent logs. Status code: {:?}", r.status),
                        );
                        Ok(())
                    } else {
                        log_to_console(
                            Level::Error,
                            "tank_sensor_level_embedded::logging::send_logs_to_server()",
                            &format_args!("Failed to send logs: Status code {:?}", r.status),
                        );
                        Err(Error::NonSuccessResponseCode)
                    }
                }
                Err(e) => {
                    log_to_console(
                        Level::Error,
                        "tank_sensor_level_embedded::logging::send_logs_to_server()",
                        &format_args!("Failed to send logs: error {:?}", e),
                    );
                    Err(Error::RequestFailed)
                }
            }
        }
        Err(_) => Err(Error::FailedToSerializeLogs),
    }
}

/// Setup logging
///
/// To change the log level change the `env` section in `.cargo/config.toml`
/// or remove it and set the environment variable `ESP_LOG` manually before
/// running `cargo run`.
///
/// This requires a clean rebuild because of
/// <https://github.com/rust-lang/cargo/issues/10358>
pub fn setup(
    spawner: Spawner,
    net_stack_provider: Receiver<'static, NoopRawMutex, Stack<'_>, 1>,
) -> Result<&'static Signal<CriticalSectionRawMutex, bool>, Error> {
    // Initialize the static buffer

    // Set the logger
    let logger_set_result = log::set_logger(LOGGER.get());
    if logger_set_result.is_err() {
        return Err(Error::FailedToSetLogger);
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

    // Spawn logger task
    spawner
        .spawn(logger_task(
            net_stack_provider,
            &LOGGER_SHUT_DOWN_REQUESTED_CHANNEL,
            &LOGGER_SHUT_DOWN_COMPLETE_CHANNEL,
        ))
        .unwrap();

    Ok(&LOGGER_SHUT_DOWN_REQUESTED_CHANNEL)
}
