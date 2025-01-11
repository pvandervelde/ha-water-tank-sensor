use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Receiver};
use log::info;
use time::OffsetDateTime;
use uom::si::{pressure::hectopascal, ratio::percent, thermodynamic_temperature::degree_celsius};

use crate::http::ClientTrait as HttpClientTrait;
use crate::sensor_data::{Reading, Sample};

/// Extend an HTTP client for sending data to Grafana
pub trait GrafanaApiClient: HttpClientTrait {
    /// Fetch the current time
    async fn upload_sensor_data(&mut self) -> Result<(), Error> {
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

    async fn send_to_grafana(
        client: &mut HttpClient<'_, WifiDevice<'static>>,
        metrics: &str,
    ) -> Result<(), &'static str> {
        let mut headers = Headers::new();
        headers.set_authorization_basic("", GRAFANA_API_KEY);
        headers.set_content_type(ContentType::TextPlain);

        let request = RequestBuilder::new(GRAFANA_CLOUD_URL)
            .method(Method::Post)
            .headers(headers)
            .body(metrics.as_bytes());

        match client.request(request).await {
            Ok(response) => {
                if response.status.is_success() {
                    Ok(())
                } else {
                    Err("Non-success status code")
                }
            }
            Err(_) => Err("Request failed"),
        }
    }

    fn format_metrics(temperature: f32, humidity: f32, pressure: f32) -> String<256> {
        let mut buffer: String<256> = String::new();

        write!(
            buffer,
            "# TYPE temperature_celsius gauge\ntemperature_celsius {}\n",
            temperature
        )
        .unwrap();
        write!(
            buffer,
            "# TYPE humidity_percent gauge\nhumidity_percent {}\n",
            humidity
        )
        .unwrap();
        write!(
            buffer,
            "# TYPE pressure_hpa gauge\npressure_hpa {}\n",
            pressure
        )
        .unwrap();

        buffer
    }
}

impl WorldTimeApiClient for HttpClient {}

/// Print a sample to log
fn log_sample(time: &OffsetDateTime, sample: &Sample) {
    let temperature = sample.temperature.get::<degree_celsius>();
    let humidity = sample.humidity.get::<percent>();
    let pressure = sample.pressure.get::<hectopascal>();

    info!("Received sample at {:?}", time);
    info!(" ┣ Temperature: {:.2} C", temperature);
    info!(" ┣ Humidity:    {:.2} %", humidity);
    info!(" ┗ Pressure:    {:.2} hPa", pressure);
}

#[embassy_executor::task]
pub async fn update_task(receiver: Receiver<'static, NoopRawMutex, Reading, 3>) {
    // For now write the data to the output

    loop {
        info!("Wait for message from sensor");
        let reading = receiver.receive().await;
        let now = reading.0;
        let sample = reading.1;

        log_sample(&now, &sample);
    }
}
