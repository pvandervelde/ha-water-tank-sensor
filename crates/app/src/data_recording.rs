use core::fmt::Write;

use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_net::{dns::DnsSocket, tcp::client::TcpClient};
use embassy_sync::channel::Sender;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Receiver};

use heapless::String;

use log::error;
use log::info;

use rand_core::RngCore as _;

use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::{
    headers::ContentType,
    request::{Method, RequestBuilder},
};

use thiserror::Error;

use time::OffsetDateTime;
use uom::si::{pressure::hectopascal, ratio::percent, thermodynamic_temperature::degree_celsius};

use crate::random::RngWrapper;
use crate::sensor_data::{EnvironmentalData, Reading};

const GRAFANA_CLOUD_URL: &str = env!("GRAFANA_METRICS_URL");
const GRAFANA_USER_NAME: &str = env!("GRAFANA_USER_NAME");
const GRAFANA_API_KEY: &str = env!("GRAFANA_METRICS_API_KEY");

/// A clock error
#[derive(Error, Debug)]
pub enum Error {
    /// Error caused when a conversion has failed.
    #[error("The conversion failed.")]
    ConversionFailed,

    #[error("The response code does not indicate success.")]
    NonSuccessResponseCode,

    #[error("The request failed to send.")]
    RequestFailed,
}

fn format_metrics(boot_count: u32, environmental_data: Reading) -> String<256> {
    let timestamp = environmental_data.0;
    let environmental_sample = environmental_data.1;

    let temperature = environmental_sample.temperature;
    let humidity = environmental_sample.humidity;
    let air_pressure = environmental_sample.pressure;

    // battery_voltage: f32,
    // pressure_sensor_voltage: f32,
    // liquid_height: f32,

    let unix_timestamp = timestamp.unix_timestamp_nanos() / 1_000_000; // Convert to milliseconds
    let mut buffer: String<256> = String::new();

    write!(
        buffer,
        "# TYPE boot_count counter\nboot_count {} {}\n",
        boot_count, unix_timestamp
    )
    .unwrap();
    write!(
        buffer,
        "# TYPE temperature_celsius gauge\ntemperature_celsius {} {}\n",
        temperature.value, unix_timestamp
    )
    .unwrap();
    write!(
        buffer,
        "# TYPE humidity_percent gauge\nhumidity_percent {} {}\n",
        humidity.value, unix_timestamp
    )
    .unwrap();
    write!(
        buffer,
        "# TYPE pressure_hpa gauge\npressure_hpa {} {}\n",
        air_pressure.value, unix_timestamp
    )
    .unwrap();

    buffer
}

/// Print a sample to log
fn log_sample(time: &OffsetDateTime, sample: &EnvironmentalData) {
    let temperature = sample.temperature.get::<degree_celsius>();
    let humidity = sample.humidity.get::<percent>();
    let pressure = sample.pressure.get::<hectopascal>();

    info!("Received sample at {:?}", time);
    info!(" ┣ Temperature: {:.2} C", temperature);
    info!(" ┣ Humidity:    {:.2} %", humidity);
    info!(" ┗ Pressure:    {:.2} hPa", pressure);
}

async fn receive_environmental_data(
    receiver: &Receiver<'static, NoopRawMutex, Reading, 3>,
) -> Result<Reading, Error> {
    info!("Wait for message from sensor");

    let reading = receiver.receive().await;
    log_sample(&reading.0, &reading.1);

    Ok(reading)
}

async fn send_data_to_grafana<'a>(
    stack: Stack<'a>,
    rng_wrapper: &mut RngWrapper,
    boot_count: u32,
    environmental_data: Reading,
) -> Result<(), Error> {
    info!("Wait for message from sensor");

    let metrics = format_metrics(boot_count, environmental_data);
    let bytes = metrics.as_bytes();

    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    let seed = rng_wrapper.next_u64();
    let mut read_record_buffer = [0_u8; 16640];
    let mut write_record_buffer = [0_u8; 16640];

    let tls_config = TlsConfig::new(
        seed,
        &mut read_record_buffer,
        &mut write_record_buffer,
        TlsVerify::None,
    );

    let mut client = HttpClient::new_with_tls(&tcp_client, &dns_socket, tls_config);

    let mut rx_buf = [0; 4096];
    let mut request = client
        .request(Method::POST, GRAFANA_CLOUD_URL)
        .await
        .unwrap()
        .basic_auth(GRAFANA_USER_NAME, GRAFANA_API_KEY)
        .content_type(ContentType::TextPlain)
        .body(bytes);
    let response = request.send(&mut rx_buf).await;

    match response {
        Ok(r) => {
            if r.status.is_successful() {
                Ok(())
            } else {
                Err(Error::NonSuccessResponseCode)
            }
        }
        Err(_) => Err(Error::RequestFailed),
    }
}

#[embassy_executor::task]
pub async fn update_task(
    stack: Stack<'static>,
    mut rng_wrapper: RngWrapper,
    environmental_data_receiver: Receiver<'static, NoopRawMutex, Reading, 3>,
    //ad_data_receiver: Receiver<'static, NoopRawMutex, Reading, 3>,
    data_sent_sender: Sender<'static, NoopRawMutex, bool, 3>,
    boot_count: u32,
) {
    info!("Waiting for sensor data to arrive ...");

    // Get data from environment sensor
    let reading = match receive_environmental_data(&environmental_data_receiver).await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to read the environmental data: {e:?}");
            return;
        }
    };

    // Get data from AD converter
    info!("Sending data ...");
    if let Err(error) = send_data_to_grafana(stack, &mut rng_wrapper, boot_count, reading).await {
        error!("Could not send data to Grafana: {error:?}");
    }

    data_sent_sender.send(true).await;
}
