use core::net::IpAddr;
use core::net::SocketAddr;
use core::num::ParseIntError;
use core::str::Utf8Error;

use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::Stack;

use embassy_time::Instant;
use esp_hal::macros::ram;

use hifitime::prelude::*;

use log::debug;
use log::error;
use log::info;

use sntpc::{get_time, NtpContext, NtpTimestampGenerator};
use thiserror::Error;

use time::error::ComponentRange as TimeComponentRange;

/// Stored boot time between deep sleep cycles
///
/// This is a statically allocated variable and it is placed in the RTC Fast
/// memory, which survives deep sleep.
#[ram(rtc_fast)]
static mut BOOT_TIME: u64 = 0;

/// Stored time indicating when the last time was that the clock was synchronized to NTP
///
/// This is a statically allocated variable and it is placed in the RTC Fast
/// memory, which survives deep sleep.
#[ram(rtc_fast)]
static mut LAST_CLOCK_UPDATE_TIME: u64 = 0;

// NTP configuration
const NTP_SERVER: &str = "pool.ntp.org";
const NTP_PORT: u16 = 123;
const NTP_SYNC_INTERVAL_IN_SECONDS: u32 = 3600; // Sync every hour

/// A clock error
#[derive(Error, Debug)]
pub enum Error {
    /// DNS error
    #[error("A DNS error occured.")]
    Dns(embassy_net::dns::Error),

    /// Error from HTTP client
    #[error("An HTTP error occured.")]
    Http(reqwless::Error),

    #[error("Invalid DNS address.")]
    InvalidDnsAddress,

    #[error("Invalid NTP time.")]
    InvalidNtpTime(sntpc::Error),

    /// An integer valued returned by the server could not be parsed
    #[error("An integer valued returned by the server could not be parsed.")]
    ParseInt(ParseIntError),

    /// A time component is out of range
    #[error("A time component is out of range.")]
    TimeComponentRange(TimeComponentRange),

    #[error("Failed to bind the UDP socket")]
    UdpBind(embassy_net::udp::BindError),

    /// Text returned by the server is not valid UTF-8
    #[error("Text returned by the server is not valid UTF-8.")]
    Utf8(Utf8Error),
}

impl From<embassy_net::dns::Error> for Error {
    fn from(value: embassy_net::dns::Error) -> Self {
        Error::Dns(value)
    }
}

impl From<embassy_net::udp::BindError> for Error {
    fn from(value: embassy_net::udp::BindError) -> Self {
        Error::UdpBind(value)
    }
}

impl From<reqwless::Error> for Error {
    fn from(value: reqwless::Error) -> Self {
        Error::Http(value)
    }
}

impl From<sntpc::Error> for Error {
    fn from(value: sntpc::Error) -> Self {
        Error::InvalidNtpTime(value)
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

#[derive(Copy, Clone, Default)]
struct Timestamp {
    duration: Duration,
}

impl NtpTimestampGenerator for Timestamp {
    fn init(&mut self) {
        self.duration = Duration::default();
    }

    fn timestamp_sec(&self) -> u64 {
        self.duration.to_seconds() as u64
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        let diff =
            (((self.duration.to_seconds() as i64) as f64) - self.duration.to_seconds()) * 1e6;
        diff as u32
    }
}

/// A clock
#[derive(Clone, Debug)]
pub struct Clock {
    epoch: Epoch,
}

impl Clock {
    /// Create a new clock
    fn new_from_utc_seconds(utc_time_in_seconds: u64) -> Self {
        let epoch = Epoch::from_utc_seconds(utc_time_in_seconds as f64);

        Self { epoch }
    }

    fn new_from_epoch(epoch: Epoch) -> Self {
        Self { epoch }
    }

    /// Return the current time
    pub fn now(&self) -> Epoch {
        self.epoch
    }

    /// Create a new clock by synchronizing with a server
    pub async fn from_server<'a>(stack: Stack<'a>) -> Result<Self, Error> {
        // Create UDP socket
        let mut rx_meta = [PacketMetadata::EMPTY; 16];
        let mut rx_buffer = [0; 4096];
        let mut tx_meta = [PacketMetadata::EMPTY; 16];
        let mut tx_buffer = [0; 4096];

        let mut socket = UdpSocket::new(
            stack,
            &mut rx_meta,
            &mut rx_buffer,
            &mut tx_meta,
            &mut tx_buffer,
        );

        socket.bind(NTP_PORT)?;

        let ntp_addrs = stack
            .dns_query(NTP_SERVER, DnsQueryType::A)
            .await
            .expect("Failed to resolve DNS");
        if ntp_addrs.is_empty() {
            error!("Failed to resolve DNS");
            return Err(Error::InvalidDnsAddress);
        }

        let context = NtpContext::new(Timestamp::default());

        // Receive response
        let addr: IpAddr = ntp_addrs[0].into();
        let result = get_time(SocketAddr::from((addr, 123)), &socket, context).await;

        match result {
            Ok(time) => {
                info!("Time: {:?}", time);
                let epoch = Epoch::from_unix_seconds(time.seconds as f64);
                let clock = Clock::new_from_epoch(epoch);

                save_last_update_time_to_rtc_memory(clock.now());

                Ok(clock)
            }
            Err(e) => {
                error!("Error getting time: {:?}", e);
                Err(Error::InvalidNtpTime(e))
            }
        }
    }

    /// Initialize clock from RTC Fast memory
    pub fn from_rtc_memory() -> Option<Self> {
        // SAFETY:
        // There is only one thread
        let now = unsafe { BOOT_TIME };
        debug!("Loading time from RTC memory. Retrieved time of: {}", now);
        if now == 0 {
            None
        } else {
            Some(Self::new_from_utc_seconds(now))
        }
    }

    /// Store clock into RTC Fast memory
    pub fn save_to_rtc_memory(&self, expected_sleep_duration: Duration) {
        let now = self.now_as_epoch();
        let then = now + expected_sleep_duration;

        // SAFETY:
        // There is only one thread
        unsafe {
            BOOT_TIME = then.to_utc_seconds() as u64;
        }
    }

    /// Return current time as a UTC epoch
    pub fn now_as_epoch(&self) -> Epoch {
        let micro_seconds_since_boot = Instant::now().as_micros();
        self.epoch + hifitime::Duration::from_microseconds(micro_seconds_since_boot as f64)
    }
}

/// Compute the duration to next wakeup rounded down to a period
fn duration_to_next_rounded_wakeup(now: Epoch, period: Duration) -> Duration {
    let then = next_rounded_wakeup(now, period);
    then - now
}

/// Load clock from RTC memory of from server
pub async fn load_clock<'a>(stack: Stack<'_>) -> Result<Clock, Error> {
    let last_restore_time = load_last_update_time_from_rtc_memory();

    let clock = if let Some(clock) = Clock::from_rtc_memory() {
        if let Some(restore_time) = last_restore_time {
            let recheck_time =
                restore_time + Duration::from_seconds(NTP_SYNC_INTERVAL_IN_SECONDS as f64);
            if clock.now() > recheck_time {
                info!(
                    "Last NTP synchronization longer than {} seconds. Synchronizing clock from NTP",
                    NTP_SYNC_INTERVAL_IN_SECONDS
                );
                Clock::from_server(stack).await?
            } else {
                info!("Clock loaded from RTC memory");
                clock
            }
        } else {
            info!("Clock loaded from RTC memory");
            clock
        }
    } else {
        info!("Synchronize clock from server");

        Clock::from_server(stack).await?
    };

    Ok(clock)
}

fn load_last_update_time_from_rtc_memory() -> Option<Epoch> {
    // SAFETY:
    // There is only one thread
    let now = unsafe { LAST_CLOCK_UPDATE_TIME };
    debug!(
        "Loading last update time from RTC memory. Retrieved time of: {}",
        now
    );
    if now == 0 {
        None
    } else {
        Some(Epoch::from_utc_seconds(now as f64))
    }
}

/// Compute the next wakeup rounded down to a period
///
/// * At 09:46:12 with period 1 minute, next rounded wakeup is 09:47:00.
/// * At 09:46:12 with period 5 minutes, next rounded wakeup is 09:50:00.
/// * At 09:46:12 with period 1 hour, next rounded wakeup is 10:00:00.
fn next_rounded_wakeup(now: Epoch, period: Duration) -> Epoch {
    let then = now + period;
    let time_in_seconds = (then.to_utc_seconds() as u64 / 60) * 60;
    Epoch::from_utc_seconds(time_in_seconds as f64)
}

fn save_last_update_time_to_rtc_memory(now: Epoch) {
    // SAFETY:
    // There is only one thread
    unsafe {
        LAST_CLOCK_UPDATE_TIME = now.to_utc_seconds() as u64;
    }
}
