use core::fmt::Write;

use embassy_net::tcp::client::TcpClientState;
use embassy_net::Stack;
use embassy_net::{dns::DnsSocket, tcp::client::TcpClient};
use esp_hal::time::now;
use heapless::String;
use log::{debug, error};
use reqwless::client::HttpClient;
use reqwless::{headers::ContentType, request::RequestBuilder};
use thiserror::Error;

use crate::device_meta::DEVICE_LOCATION;

const METRICS_URL: &str = env!("METRICS_URL");

/// Errors that can occur when sending timing data
#[derive(Error, Debug)]
pub enum Error {
    #[error("The response code does not indicate success.")]
    NonSuccessResponseCode,

    #[error("The request failed to send.")]
    RequestFailed,
}

fn format_timing_data(boot_count: u32, ticks_in_micro_seconds: u64) -> String<256> {
    let mut buffer: String<256> = String::new();

    writeln!(
        buffer,
        "{{\"device_id\":\"{device_id}\",\"boot_count\":{boot_count},\"timestamp\":{ticks:.3}}}",
        device_id = DEVICE_LOCATION,
        boot_count = boot_count,
        ticks = (ticks_in_micro_seconds as f64) * 1e-6,
    )
    .unwrap();

    buffer
}

/// Send timing data to the server immediately after WiFi connection
pub async fn send_timing_data(stack: Stack<'_>, boot_count: u32) -> Result<(), Error> {
    debug!("Sending timing data...");

    let timing_data = format_timing_data(boot_count, now().ticks());
    let bytes = timing_data.as_bytes();

    let dns_socket = DnsSocket::new(stack);
    let tcp_client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &tcp_client_state);

    debug!("Creating HTTP client...");
    let mut client = HttpClient::new(&tcp_client, &dns_socket);

    debug!("Creating request...");
    let mut rx_buf = [0; 4096];
    let mut resource = client.resource(METRICS_URL).await.unwrap();
    let response = resource
        .post("/api/v1/timing")
        .content_type(ContentType::ApplicationJson)
        .body(bytes);

    debug!("Sending request...");
    let response = response.send(&mut rx_buf).await;

    debug!("Processing response...");
    match response {
        Ok(r) => {
            if r.status.is_successful() {
                debug!("Sent timing data. Status code: {:?}", r.status);
                Ok(())
            } else {
                error!("Failed to send timing data: Status code {:?}", r.status);
                Err(Error::NonSuccessResponseCode)
            }
        }
        Err(e) => {
            error!("Failed to send timing data: error {:?}", e);
            Err(Error::RequestFailed)
        }
    }
}
