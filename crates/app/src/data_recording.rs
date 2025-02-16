use core::fmt::Write;

use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_net::{dns::DnsSocket, tcp::client::TcpClient};
use embassy_sync::channel::Sender;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Receiver};

use esp_hal::peripherals::TIMG1;
use esp_hal::time::{now, Instant};
use heapless::String;

use hifitime::Epoch;
use log::info;
use log::{debug, error};

use rand_core::RngCore as _;

use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::{
    headers::ContentType,
    request::{Method, RequestBuilder},
};

use thiserror::Error;

use uom::si::angle::degree;
use uom::si::pressure::pascal;
use uom::si::{pressure::hectopascal, ratio::percent, thermodynamic_temperature::degree_celsius};

use crate::device_meta::DEVICE_LOCATION;
use crate::meta::CARGO_PKG_VERSION;
use crate::random::RngWrapper;
use crate::sensor_data::{EnvironmentalData, Reading};

const METRICS_URL: &str = env!("METRICS_URL");
//const GRAFANA_USER_NAME: &str = env!("GRAFANA_USER_NAME");
//const GRAFANA_API_KEY: &str = env!("GRAFANA_METRICS_API_KEY");

/// A clock error
#[derive(Error, Debug)]
pub enum Error {
    #[error("The response code does not indicate success.")]
    NonSuccessResponseCode,

    #[error("The request failed to send.")]
    RequestFailed,
}

// Use the influx line protocol from here: https://docs.influxdata.com/influxdb/v1/write_protocols/line_protocol_tutorial/
fn format_metrics(
    boot_count: u32,
    environmental_data: Reading,
    run_time_in_micro_seconds: u64,
) -> String<512> {
    let timestamp = environmental_data.0;
    let environmental_sample = environmental_data.1;

    let temperature = environmental_sample.temperature;
    let humidity = environmental_sample.humidity;
    let air_pressure = environmental_sample.pressure;

    // battery_voltage: f32,
    // pressure_sensor_voltage: f32,
    // liquid_height: f32,
    // liquid_temperature: f32

    // The influx timestamp should be in nano seconds
    let unix_timestamp = timestamp.to_unix_milliseconds() * 1e-3;
    let mut buffer: String<512> = String::new();

    writeln!(
        buffer,
        "{{\"device_id\":\"{device_id}\",\"firmware_version\":\"{firmware_version}\",\"boot_count\":{boot_count},\"unix_time_in_seconds\":{unix_timestamp},\"run_time_in_seconds\":{run_time:.3},\"temperature_in_celcius\":{temperature:.2},\"humidity_in_percent\":{humidity:.2},\"pressure_in_pascal\":{pressure:.1},\"battery_voltage\":{battery_voltage:.3},\"pressure_sensor_voltage\":{pressure_sensor_voltage:.3},\"tank_level_in_meters\":{tank_level:.3},\"tank_temperature_in_celcius\":{tank_temperature:.2}}}",
        device_id=DEVICE_LOCATION,
        firmware_version=CARGO_PKG_VERSION.unwrap_or("NOT FOUND"),
        boot_count=boot_count,
        unix_timestamp=unix_timestamp,
        run_time=(run_time_in_micro_seconds as f64) * 1e-6,
        temperature=temperature.get::<degree_celsius>(),
        humidity=humidity.get::<percent>(),
        pressure=air_pressure.get::<pascal>(),
        battery_voltage=0.0,
        pressure_sensor_voltage=0.0,
        tank_level=0.0,
        tank_temperature=temperature.get::<degree_celsius>(),
    )
    .unwrap();

    buffer
}

/// Print a sample to log
fn log_sample(time: &Epoch, sample: &EnvironmentalData) {
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

async fn send_metrics<'a>(
    stack: Stack<'a>,
    rng_wrapper: &mut RngWrapper,
    boot_count: u32,
    environmental_data: Reading,
    run_time_in_micro_seconds: u64,
) -> Result<(), Error> {
    info!("Sending metrics ...");

    let metrics = format_metrics(boot_count, environmental_data, run_time_in_micro_seconds);
    let bytes = metrics.as_bytes();

    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    // let seed = rng_wrapper.next_u64();
    // let mut read_record_buffer = [0_u8; 16640];
    // let mut write_record_buffer = [0_u8; 16640];

    // let tls_config = TlsConfig::new(
    //     seed,
    //     &mut read_record_buffer,
    //     &mut write_record_buffer,
    //     TlsVerify::None,
    // );

    debug!("Creating HTTP client ...");
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    debug!("Creating request ...");
    let mut rx_buf = [0; 4096];
    let mut resource = client.resource(METRICS_URL).await.unwrap();
    let response = resource
        .post("/api/v1/sensor")
        .content_type(ContentType::ApplicationJson)
        //.basic_auth(GRAFANA_USER_NAME, GRAFANA_API_KEY)
        .body(bytes);

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

#[embassy_executor::task]
pub async fn update_task(
    stack: Stack<'static>,
    mut rng_wrapper: RngWrapper,
    environmental_data_receiver: Receiver<'static, NoopRawMutex, Reading, 3>,
    //ad_data_receiver: Receiver<'static, NoopRawMutex, Reading, 3>,
    data_sent_sender: Sender<'static, NoopRawMutex, bool, 3>,
    boot_count: u32,
    system_start_time: Instant,
) {
    // Get data from environment sensor
    let reading = match receive_environmental_data(&environmental_data_receiver).await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to read the environmental data: {e:?}");
            return;
        }
    };

    // Get duration for operations
    let current_time = now();
    let run_time_in_micro_seconds = current_time
        .checked_duration_since(system_start_time)
        .unwrap()
        .to_micros();

    // Get data from AD converter
    if let Err(error) = send_metrics(
        stack,
        &mut rng_wrapper,
        boot_count,
        reading,
        run_time_in_micro_seconds,
    )
    .await
    {
        error!("Could not send metrics: {error:?}");
    }

    data_sent_sender.send(true).await;
}
