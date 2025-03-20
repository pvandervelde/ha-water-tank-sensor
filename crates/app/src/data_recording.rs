use core::fmt::Write;

use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_net::{dns::DnsSocket, tcp::client::TcpClient};

use embassy_time::Duration;
use esp_hal::time::{now, Instant};
use heapless::String;

use log::info;
use log::{debug, error};

use reqwless::client::HttpClient;
use reqwless::{headers::ContentType, request::RequestBuilder};

use thiserror::Error;

use uom::si::electric_potential::volt;
use uom::si::length::meter;
use uom::si::pressure::pascal;
use uom::si::{pressure::hectopascal, ratio::percent, thermodynamic_temperature::degree_celsius};

use crate::device_meta::DEVICE_LOCATION;
use crate::meta::CARGO_PKG_VERSION;
use crate::sensor_data::{Ads1115Data, Bme280Data};
use crate::wifi::DEFAULT_TCP_TIMEOUT_IN_MILLISECONDS;

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
    bme280_data: Bme280Data,
    ads1115_data: Ads1115Data,
    run_time_in_micro_seconds: u64,
    wifi_start_time: u64,
) -> String<512> {
    let temperature = bme280_data.temperature;
    let humidity = bme280_data.humidity;
    let air_pressure = bme280_data.pressure;

    let brightness = ads1115_data.enclosure_relative_brightness;
    let battery_voltage = ads1115_data.battery_voltage;
    let pressure_sensor_voltage = ads1115_data.pressure_sensor_voltage;
    let liquid_height = ads1115_data.height_above_sensor;
    // liquid_temperature: f32

    // The influx timestamp should be in nano seconds
    let mut buffer: String<512> = String::new();

    writeln!(
        buffer,
        "{{\"device_id\":\"{device_id}\",\"firmware_version\":\"{firmware_version}\",\"boot_count\":{boot_count},\"run_time_in_seconds\":{run_time:.3},\"wifi_start_time_in_seconds\":{wifi_start_time:.3},\"temperature_in_celcius\":{temperature:.2},\"humidity_in_percent\":{humidity:.2},\"pressure_in_pascal\":{pressure:.1},\"brightness_in_percent\":{brightness:.3},\"battery_voltage\":{battery_voltage:.3},\"pressure_sensor_voltage\":{pressure_sensor_voltage:.3},\"tank_level_in_meters\":{tank_level:.3},\"tank_temperature_in_celcius\":{tank_temperature:.2}}}",
        device_id=DEVICE_LOCATION,
        firmware_version=CARGO_PKG_VERSION.unwrap_or("NOT FOUND"),
        boot_count=boot_count,
        run_time=(run_time_in_micro_seconds as f64) * 1e-6,
        wifi_start_time = (wifi_start_time as f64) * 1e-6,
        temperature=temperature.get::<degree_celsius>(),
        humidity=humidity.get::<percent>(),
        pressure=air_pressure.get::<pascal>(),
        brightness=brightness.get::<percent>(),
        battery_voltage=battery_voltage.get::<volt>(),
        pressure_sensor_voltage=pressure_sensor_voltage.get::<volt>(),
        tank_level=liquid_height.get::<meter>(),
        tank_temperature=temperature.get::<degree_celsius>(),
    )
    .unwrap();

    buffer
}

fn log_ads1115_reading(sample: &Ads1115Data) {
    let battery_voltage = sample.battery_voltage.get::<volt>();
    let pressure_sensor_voltage = sample.pressure_sensor_voltage.get::<volt>();
    let height_above_sensor = sample.height_above_sensor.get::<meter>();

    info!(" ┣ Battery voltage:            {:.2} V", battery_voltage);
    info!(
        " ┣ Pressure sensor voltage:    {:.2} V",
        pressure_sensor_voltage
    );
    info!(
        " ┗ Liquid height above sensor: {:.2} m",
        height_above_sensor
    );
}

fn log_bme280_reading(sample: &Bme280Data) {
    let temperature = sample.temperature.get::<degree_celsius>();
    let humidity = sample.humidity.get::<percent>();
    let pressure = sample.pressure.get::<hectopascal>();

    info!(" ┣ Temperature: {:.2} C", temperature);
    info!(" ┣ Humidity:    {:.2} %", humidity);
    info!(" ┗ Pressure:    {:.2} hPa", pressure);
}

pub async fn send_metrics_to_server(
    stack: Stack<'static>,
    bme280_reading: Bme280Data,
    ads1115_reading: Ads1115Data,
    boot_count: u32,
    system_start_time: Instant,
    wifi_start_time: u64,
) -> Result<(), Error> {
    info!("Sending metrics to server ...");

    let current_time = now();
    let run_time_in_micro_seconds = current_time
        .checked_duration_since(system_start_time)
        .unwrap()
        .to_micros();

    log_ads1115_reading(&ads1115_reading);
    log_bme280_reading(&bme280_reading);

    let metrics = format_metrics(
        boot_count,
        bme280_reading,
        ads1115_reading,
        run_time_in_micro_seconds,
        wifi_start_time,
    );
    let bytes = metrics.as_bytes();

    let dns_socket = DnsSocket::new(stack);

    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let mut tcp_client = TcpClient::new(stack, &tcp_client_state);
    tcp_client.set_timeout(Some(Duration::from_millis(
        DEFAULT_TCP_TIMEOUT_IN_MILLISECONDS,
    )));

    debug!("Creating HTTP client ...");
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    debug!("Creating request ...");
    let mut rx_buf = [0; 4096];
    let mut resource = client.resource(METRICS_URL).await.unwrap();
    let response = resource
        .post("/api/v1/sensor")
        .content_type(ContentType::ApplicationJson)
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
