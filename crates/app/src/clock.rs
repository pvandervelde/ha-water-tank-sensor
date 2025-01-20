// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Data types and function for keeping time and synchronizing clock

use core::num::ParseIntError;
use core::str::from_utf8;
use core::str::Utf8Error;

use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::TcpClient;
use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_time::Duration;
use embassy_time::Instant;

use esp_hal::macros::ram;

use heapless::Vec;

use log::debug;
use log::error;
use log::trace;

use rand_core::RngCore as _;

use reqwless::client::HttpClient;
use reqwless::client::TlsConfig;
use reqwless::client::TlsVerify;
use reqwless::request::Method;

use thiserror::Error;

use time::error::ComponentRange as TimeComponentRange;
use time::OffsetDateTime;
use time::UtcOffset;

use crate::http::HTTP_RESPONSE_SIZE;
use crate::random::RngWrapper;

/// Stored boot time between deep sleep cycles
///
/// This is a statically allocated variable and it is placed in the RTC Fast
/// memory, which survives deep sleep.
#[ram(rtc_fast)]
static mut BOOT_TIME: (u64, i32) = (0, 0);

/// A clock error
#[derive(Error, Debug)]
pub enum Error {
    /// Error from HTTP client
    #[error("An HTTP error occured.")]
    Http(reqwless::Error),

    /// The time is invalid in the current time offset
    #[error("The time is invalid in the current time offset.")]
    InvalidInOffset,

    /// An integer valued returned by the server could not be parsed
    #[error("An integer valued returned by the server could not be parsed.")]
    ParseInt(ParseIntError),

    /// Response was too large
    #[error("Response was too large.")]
    ResponseTooLarge,

    /// Error synchronizing time from World Time API
    #[error("Error synchronizing time from World Time API.")]
    Synchronization,

    /// A time component is out of range
    #[error("A time component is out of range.")]
    TimeComponentRange(TimeComponentRange),

    /// Current time could not be fetched
    #[error("Current time could not be fetched.")]
    Unknown,

    /// Text returned by the server is not valid UTF-8
    #[error("Text returned by the server is not valid UTF-8.")]
    Utf8(Utf8Error),
}

impl From<reqwless::Error> for Error {
    fn from(value: reqwless::Error) -> Self {
        Error::Http(value)
    }
}

impl From<ParseIntError> for Error {
    fn from(value: ParseIntError) -> Self {
        Error::ParseInt(value)
    }
}

impl From<TimeComponentRange> for Error {
    fn from(value: TimeComponentRange) -> Self {
        Error::TimeComponentRange(value)
    }
}

impl From<Utf8Error> for Error {
    fn from(value: Utf8Error) -> Self {
        Error::Utf8(value)
    }
}

/// A clock
#[derive(Clone, Debug)]
pub struct Clock {
    /// The boot time in Unix epoch
    boot_time: u64,

    /// The time offset
    offset: UtcOffset,
}

impl Clock {
    /// Create a new clock
    pub fn new(current_time: u64, offset: UtcOffset) -> Self {
        let from_boot = Instant::now().as_secs();
        let boot_time = current_time - from_boot;

        Self { boot_time, offset }
    }

    /// Return the current time
    pub fn now(&self) -> Result<OffsetDateTime, Error> {
        let epoch = self.now_as_epoch();
        #[expect(clippy::cast_possible_wrap, reason = "Timestamp will fit an i64")]
        let utc = OffsetDateTime::from_unix_timestamp(epoch as i64)?;
        let local = utc
            .checked_to_offset(self.offset)
            .ok_or(Error::InvalidInOffset)?;
        Ok(local)
    }

    /// Create a new clock by synchronizing with a server
    pub async fn from_server<'a>(
        stack: Stack<'a>,
        rng_wrapper: &mut RngWrapper,
    ) -> Result<Self, Error> {
        let response = request_current_time(stack, rng_wrapper).await?;
        let now = process_worldtimeapi_response(response)?;

        let current_time = now.unix_timestamp();

        #[expect(
            clippy::cast_sign_loss,
            reason = "Current timestamp will never be negative"
        )]
        let current_time = current_time as u64;

        let offset = now.offset();

        Ok(Self::new(current_time, offset))
    }

    /// Initialize clock from RTC Fast memory
    pub fn from_rtc_memory() -> Option<Self> {
        // SAFETY:
        // There is only one thread
        let (now, offset_in_seconds) = unsafe { BOOT_TIME };
        let offset = UtcOffset::from_whole_seconds(offset_in_seconds).ok();

        if now == 0 {
            None
        } else {
            offset.map(|offset| Self::new(now, offset))
        }
    }

    /// Store clock into RTC Fast memory
    pub fn save_to_rtc_memory(&self, expected_sleep_duration: Duration) {
        let now = self.now_as_epoch();
        let then = now + expected_sleep_duration.as_secs();
        let offset_in_seconds = self.offset.whole_seconds();
        // SAFETY:
        // There is only one thread
        unsafe {
            BOOT_TIME = (then, offset_in_seconds);
        }
    }

    /// Compute the next wakeup rounded down to a period
    ///
    /// * At 09:46:12 with period 1 minute, next rounded wakeup is 09:47:00.
    /// * At 09:46:12 with period 5 minutes, next rounded wakeup is 09:50:00.
    /// * At 09:46:12 with period 1 hour, next rounded wakeup is 10:00:00.
    pub fn duration_to_next_rounded_wakeup(&self, period: Duration) -> Duration {
        let epoch = Duration::from_secs(self.now_as_epoch());
        duration_to_next_rounded_wakeup(epoch, period)
    }

    /// Return current time as a Unix epoch
    pub fn now_as_epoch(&self) -> u64 {
        let from_boot = Instant::now().as_secs();
        self.boot_time + from_boot
    }
}

/// Compute the duration to next wakeup rounded down to a period
fn duration_to_next_rounded_wakeup(now: Duration, period: Duration) -> Duration {
    let then = next_rounded_wakeup(now, period);
    then - now
}

/// Compute the next wakeup rounded down to a period
///
/// * At 09:46:12 with period 1 minute, next rounded wakeup is 09:47:00.
/// * At 09:46:12 with period 5 minutes, next rounded wakeup is 09:50:00.
/// * At 09:46:12 with period 1 hour, next rounded wakeup is 10:00:00.
fn next_rounded_wakeup(now: Duration, period: Duration) -> Duration {
    let then = now + period;
    Duration::from_secs((then.as_secs() / period.as_secs()) * period.as_secs())
}

fn process_worldtimeapi_response(
    response: Vec<u8, { HTTP_RESPONSE_SIZE }>,
) -> Result<OffsetDateTime, Error> {
    let text = from_utf8(&response)?;
    let mut timestamp: Option<u64> = None;
    let mut offset: Option<i32> = None;
    for line in text.lines() {
        trace!("Line: \"{line}\"");
        if let Some(timestamp_string) = line.strip_prefix("unixtime: ") {
            debug!("Parse line \"{line}\"");
            let timestamp_: u64 = timestamp_string.parse()?;

            debug!("Current time is {timestamp_}");
            timestamp = Some(timestamp_);
        }
        if let Some(offset_string) = line.strip_prefix("raw_offset: ") {
            debug!("Parse line \"{line}\"");
            let offset_: i32 = offset_string.parse()?;

            debug!("Current offset is {offset_}");
            offset = Some(offset_);
        }
    }

    if let (Some(timestamp), Some(offset)) = (timestamp, offset) {
        let offset = UtcOffset::from_whole_seconds(offset)?;

        #[allow(clippy::cast_possible_wrap, reason = "Timestamp will fit an i64")]
        let timestamp = timestamp as i64;

        let utc = OffsetDateTime::from_unix_timestamp(timestamp)?;
        let local = utc
            .checked_to_offset(offset)
            .ok_or(Error::InvalidInOffset)?;
        Ok(local)
    } else {
        Err(Error::Unknown)
    }
}

async fn request_current_time<'a>(
    stack: Stack<'a>,
    rng_wrapper: &mut RngWrapper,
) -> Result<Vec<u8, { HTTP_RESPONSE_SIZE }>, Error> {
    let url = "https://worldtimeapi.org/api/timezone/Pacific/Auckland.txt";

    debug!("Creating the DNS socket");
    let dns_socket = DnsSocket::new(stack);

    debug!("Creating the TCP client");
    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    let seed = rng_wrapper.next_u64();
    let mut read_record_buffer = [0_u8; 16640];
    let mut write_record_buffer = [0_u8; 16640];

    debug!("Creating the TLS config");
    let tls_config = TlsConfig::new(
        seed,
        &mut read_record_buffer,
        &mut write_record_buffer,
        TlsVerify::None,
    );

    debug!("Creating the HTTP client");
    let mut client = HttpClient::new_with_tls(&tcp_client, &dns_socket, tls_config);

    debug!("Creating the request ...");
    let request_result = client.request(Method::GET, url).await;
    let mut request = match request_result {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to create the request: {:?}", e);
            return Err(Error::Http(e));
        }
    };

    let mut buffer = [0_u8; 4096];

    debug!("Send HTTP request");
    let response = request.send(&mut buffer).await?;

    debug!("Response status: {:?}", response.status);

    let buffer = response.body().read_to_end().await?;

    debug!("Read {} bytes", buffer.len());

    let output = Vec::<u8, { HTTP_RESPONSE_SIZE }>::from_slice(buffer)
        .map_err(|()| Error::ResponseTooLarge)?;

    Ok(output)
}
