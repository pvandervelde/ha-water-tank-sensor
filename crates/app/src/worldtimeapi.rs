// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Client for World Time API

use core::num::ParseIntError;
use core::str::from_utf8;
use core::str::Utf8Error;

use log::debug;
use log::trace;

use time::error::ComponentRange as TimeComponentRangeError;
use time::OffsetDateTime;
use time::UtcOffset;

use crate::http::Client as HttpClient;
use crate::http::ClientTrait as HttpClientTrait;
use crate::http::Error as HttpError;

/// Extend an HTTP client for querying World Time API
pub trait WorldTimeApiClient: HttpClientTrait {
    /// Fetch the current time
    async fn fetch_current_time(&mut self) -> Result<OffsetDateTime, Error> {
        let url = "https://worldtimeapi.org/api/timezone/Pacific/Auckland.txt";

        let response = self.send_request(url).await?;

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
}

impl WorldTimeApiClient for HttpClient {}

/// An error within a request to World Time API
#[derive(Debug)]
pub enum Error {
    /// Current timestamp is invalid in this offset
    InvalidInOffset,

    /// Current time could not be fetched
    Unknown,

    /// A time component is out of range
    TimeComponentRange(#[expect(unused, reason = "Never read directly")] TimeComponentRangeError),

    /// Error from HTTP client
    Http(#[expect(unused, reason = "Never read directly")] HttpError),

    /// An integer valued returned by the server could not be parsed
    ParseInt(#[expect(unused, reason = "Never read directly")] ParseIntError),

    /// Text returned by the server is not valid UTF-8
    Utf8(#[expect(unused, reason = "Never read directly")] Utf8Error),
}

impl From<TimeComponentRangeError> for Error {
    fn from(error: TimeComponentRangeError) -> Self {
        Self::TimeComponentRange(error)
    }
}

impl From<HttpError> for Error {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

impl From<ParseIntError> for Error {
    fn from(error: ParseIntError) -> Self {
        Self::ParseInt(error)
    }
}

impl From<Utf8Error> for Error {
    fn from(error: Utf8Error) -> Self {
        Self::Utf8(error)
    }
}
