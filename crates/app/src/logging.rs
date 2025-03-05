// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions for setting up the logging system

use core::fmt::Write;
use core::str::FromStr;

use embassy_executor::Spawner;
use embassy_net::dns::DnsQueryType;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::TcpClient;
use embassy_net::tcp::client::TcpClientState;
use embassy_net::tcp::TcpSocket;
use embassy_net::IpEndpoint;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::channel::Receiver;
use embassy_sync::channel::Sender;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use embassy_time::Timer;
use heapless::String;
use heapless::Vec;
use log::debug;
use log::error;
use log::trace;
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
use static_cell::StaticCell;
use thiserror::Error;

// Constants for buffer sizes
const MAX_STORED_LOGS: usize = 100;
const MAX_LOG_LENGTH: usize = 256;
const HTTP_BUFFER_SIZE: usize = 2048;

// HTTP specific constants
const LOGGING_URL: &str = env!("LOGGING_URL");
const LOGGING_URL_SUB_PATH: &str = "/api/v1/logs";

// Create static channels for logger communication
static LOGGER_SHUT_DOWN_CHANNEL: Signal<CriticalSectionRawMutex, bool> = Signal::new();
static LOGGER_STATUS_SIGNAL: Signal<CriticalSectionRawMutex, LoggerStatus> = Signal::new();

static LOGGER: StaticCell<HttpLogger> = StaticCell::new();

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to serialize the logs.")]
    FailedToSerializeLogs,

    #[error("Failed to set the global logger. No logs will be provided.")]
    FailedToSetLogger,

    #[error("The POST request to send logs resulted in a non-success response code.")]
    NonSuccessResponseCode,

    #[error("The log sending request failed")]
    RequestFailed,
}

// Logger status for reporting back to main task
#[derive(Clone, Copy, PartialEq)]
pub enum LoggerStatus {
    Running,
    Idle,
    ShutdownComplete,
}

// Log entry structure
#[derive(Clone, Serialize)]
struct LogEntry {
    level: Level,
    message: String<MAX_LOG_LENGTH>,
    timestamp: u64, // Simple timestamp (milliseconds since boot)
}

// HTTP Logger implementation
pub struct HttpLogger {
    log_buffer: heapless::Deque<LogEntry, MAX_STORED_LOGS>,
    //command_sender: Option<Sender<'static, CriticalSectionRawMutex, bool, 4>>,
    status_signal: Option<&'static Signal<CriticalSectionRawMutex, LoggerStatus>>,
    current_time_ms: u64,
}

impl HttpLogger {
    pub fn new() -> Self {
        Self {
            log_buffer: heapless::Deque::new(),
            //command_sender: None,
            status_signal: None,
            current_time_ms: 0,
        }
    }

    // Initialize the logger with communication channels
    pub fn init(
        &mut self,
        //command_sender: Sender<'static, CriticalSectionRawMutex, bool, 4>,
        status_signal: &'static Signal<CriticalSectionRawMutex, LoggerStatus>,
    ) {
        //self.command_sender = Some(command_sender);
        self.status_signal = Some(status_signal);
    }

    // Call this regularly to update the internal timestamp
    pub fn update_time(&mut self, current_time_ms: u64) {
        self.current_time_ms = current_time_ms;
    }

    // // Signal shutdown to the logger task
    // pub async fn request_shutdown(&self) -> Result<(), ()> {
    //     let result = if let Some(sender) = &self.command_sender {
    //         sender.try_send(true).map_err(|_| ())
    //     } else {
    //         Err(())
    //     };

    //     if result.is_err() {
    //         return result;
    //     }

    //     if let Some(signal) = self.status_signal {
    //         // Wait until the status is ShutdownComplete
    //         while signal.wait().await != LoggerStatus::ShutdownComplete {
    //             Timer::after(Duration::from_millis(10)).await;
    //         }
    //         Ok(())
    //     } else {
    //         Err(())
    //     }
    // }

    // Store a log entry in the buffer
    fn store_log(&mut self, record: &Record) -> Result<(), &'static str> {
        let level = record.level();

        // Format the log message
        let mut message = String::new();
        let _ = write!(message, "{}", record.args());

        // Create the log entry
        let entry = LogEntry {
            level,
            message,
            timestamp: self.current_time_ms,
        };

        // Try to store the log, removing oldest entry if full
        if self.log_buffer.is_full() {
            let _ = self.log_buffer.pop_front();
        }

        if self.log_buffer.push_back(entry).is_err() {
            return Err("Failed to store log entry");
        }

        Ok(())
    }

    // Take logs from the buffer, returns number of logs taken
    pub fn take_logs(
        &mut self,
        dest: &mut Vec<LogEntry, MAX_STORED_LOGS>,
        max_count: usize,
    ) -> usize {
        let mut count = 0;

        while count < max_count && !self.log_buffer.is_empty() {
            if let Some(entry) = self.log_buffer.pop_front() {
                if dest.push(entry.clone()).is_err() {
                    // If dest is full, put the entry back and stop
                    let _ = self.log_buffer.push_front(entry);
                    break;
                }
                count += 1;
            }
        }

        count
    }
}

// Implement the Log trait for HttpLogger
impl Log for HttpLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
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

        let color = match record.level() {
            Level::Error => RED,
            Level::Warn => YELLOW,
            Level::Info => GREEN,
            Level::Debug => BLUE,
            Level::Trace => CYAN,
        };

        if self.enabled(record.metadata()) {
            // Create a mutable reference to self
            // This is unsafe but necessary due to the Log trait API design
            let this = unsafe { &mut *(self as *const Self as *mut Self) };

            // Store the log entry
            let _ = this.store_log(record);

            println!(
                "{}{:>5} {}{}{}{}]{} {}",
                color,
                record.level(),
                RESET,
                GRAY,
                record.target(),
                GRAY,
                RESET,
                record.args()
            );
        }
    }

    fn flush(&self) {
        // The Log trait's flush method doesn't have access to the async context
        // Actual flushing happens in the logger task
    }
}

// Escape JSON string (simple implementation)
fn escape_json(s: &str) -> String<512> {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => {
                let _ = result.push_str("\\\"");
            }
            '\\' => {
                let _ = result.push_str("\\\\");
            }
            '\n' => {
                let _ = result.push_str("\\n");
            }
            '\r' => {
                let _ = result.push_str("\\r");
            }
            '\t' => {
                let _ = result.push_str("\\t");
            }
            _ => {
                let _ = result.push(c);
            }
        }
    }
    result
}

// Increase retry interval with exponential backoff
fn increase_retry_interval(interval: &mut u64) {
    const MAX_RETRY_INTERVAL: u64 = 600000; // 10 minutes
    *interval = (*interval * 2).min(MAX_RETRY_INTERVAL);
}

#[embassy_executor::task]
pub async fn logger_task(
    net_stack_provider: Receiver<'static, NoopRawMutex, Stack<'static>, 1>,
    command_receiver: &'static Signal<CriticalSectionRawMutex, bool>,
    status_signal: &'static Signal<CriticalSectionRawMutex, LoggerStatus>,
) {
    let mut shutdown_requested = false;
    let mut temp_log_buffer: Vec<LogEntry, MAX_STORED_LOGS> = Vec::new();
    let mut retry_interval = 10000; // 10 seconds
    let mut last_send_attempt = 0;

    let mut stack: Option<Stack> = None;

    // Set initial status
    status_signal.signal(LoggerStatus::Idle);

    loop {
        // Check for commands
        shutdown_requested = command_receiver.signaled();
        //     match cmd {
        //         LoggerCommand::Shutdown => {
        //              = true;
        //             // Try to send any remaining logs before shutting down
        //             if network_available {
        //                 status_signal.signal(LoggerStatus::Running);
        //             }
        //         }
        //     }
        // }

        if !stack.is_some() {
            let potential_stack = net_stack_provider.try_receive();
            if let Ok(net_stack) = potential_stack {
                stack = Some(net_stack);
            } else {
                // Network stack not available
                status_signal.signal(LoggerStatus::Idle);
            }
        } else {
            let current_time = embassy_time::Instant::now().as_millis();
            let time_since_last_attempt = current_time.saturating_sub(last_send_attempt);

            // Take logs from the main buffer if our temp buffer is empty
            if temp_log_buffer.is_empty() {
                let this = unsafe { &mut *(logger as *const HttpLogger as *mut HttpLogger) };
                let _ = this.take_logs(&mut temp_log_buffer, MAX_STORED_LOGS);
            }

            // If we have logs to send and enough time has passed
            if !temp_log_buffer.is_empty() && time_since_last_attempt >= retry_interval {
                status_signal.signal(LoggerStatus::Running);
                last_send_attempt = current_time;

                // Try to send logs
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
                status_signal.signal(LoggerStatus::Idle);

                // If shutdown is requested and we've sent all logs, complete shutdown
                if shutdown_requested {
                    status_signal.signal(LoggerStatus::ShutdownComplete);
                    break;
                }
            }
        }

        // If shutdown is requested but network is not available, just complete shutdown
        if shutdown_requested {
            status_signal.signal(LoggerStatus::ShutdownComplete);
            break;
        }

        // Wait a bit before checking again
        Timer::after(Duration::from_millis(100)).await;
    }
}

// Function to send logs via HTTP
async fn send_logs_to_server(logs: &[LogEntry], stack: Stack<'_>, url: &str) -> Result<(), Error> {
    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    debug!("Creating HTTP client ...");
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    // Convert logs to JSON using serde_json_core (heapless)
    let mut json_buffer = [0u8; 2048];
    let logs_slice = if logs.len() > 10 { &logs[0..10] } else { logs }; // Limit batch size

    let mut rx_buf = [0; 4096];
    match serde_json_core::to_slice(logs_slice, &mut json_buffer) {
        Ok(size) => {
            let mut resource = client.resource(url).await.unwrap();
            let response = resource
                .post(LOGGING_URL_SUB_PATH)
                .content_type(ContentType::ApplicationJson)
                .body(&json_buffer[..size]);

            debug!("Sending request ...");
            let response = response.send(&mut rx_buf).await;

            debug!("Processing response ...");
            match response {
                Ok(r) => {
                    if r.status.is_successful() {
                        debug!("Sent metrics. Status code: {:?}", r.status);
                        Ok(())
                    } else {
                        error!("Failed to send metrics: Status code {:?}", r.status,);
                        Err(Error::NonSuccessResponseCode)
                    }
                }
                Err(e) => {
                    error!("Failed to send metrics: error {:?}", e);
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
    // Create the logger

    // Use an unsafe call so that we can change the static LOGGER item
    let mut internal = HttpLogger::new();

    // Initialize with communication channels
    internal.init(&LOGGER_STATUS_SIGNAL);

    // Set the logger
    let logger_set_result = log::set_logger(&internal);
    if logger_set_result.is_err() {
        // Could not set default logger.
        // There is nothing we can do; logging will not work.
        return Err(Error::FailedToSetLogger);
    }

    /// Log level
    const LEVEL: Option<&'static str> = option_env!("ESP_LOG");
    if let Some(level) = LEVEL {
        let level = LevelFilter::from_str(level).unwrap_or(LevelFilter::Off);

        log::set_max_level(level);
    }

    trace!("Logger is ready");

    LOGGER.init(internal);

    // Spawn logger task
    spawner
        .spawn(logger_task(
            net_stack_provider,
            &LOGGER_SHUT_DOWN_CHANNEL,
            &LOGGER_STATUS_SIGNAL,
        ))
        .unwrap();

    Ok(&LOGGER_SHUT_DOWN_CHANNEL)
}
