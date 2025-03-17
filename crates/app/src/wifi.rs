// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions and task for WiFi connection

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use esp_wifi::config::PowerSaveMode;
use log::debug;
use log::error;
use log::info;

use embassy_executor::Spawner;

use embassy_net::new as new_network_stack;

use esp_wifi::init as initialize_wifi;
use esp_wifi::wifi::new_with_mode as new_wifi_with_mode;
use esp_wifi::wifi::ClientConfiguration;
use esp_wifi::wifi::Configuration;
use esp_wifi::wifi::WifiController;
use esp_wifi::wifi::WifiDevice;
use esp_wifi::wifi::WifiError as EspWifiError;
use esp_wifi::wifi::WifiStaDevice;
use esp_wifi::EspWifiController;
use esp_wifi::InitializationError as WifiInitializationError;

use embassy_net::Config;
use embassy_net::DhcpConfig;
use embassy_net::Runner;
use embassy_net::Stack;
use embassy_net::StackResources;

use embassy_time::Duration;
use embassy_time::Timer;

use esp_hal::peripherals::RADIO_CLK;
use esp_hal::peripherals::TIMG0;
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;

use heapless::String;

use thiserror::Error;

use static_cell::StaticCell;

use rand_core::RngCore as _;

use crate::RngWrapper;

// Constants
/// Maximum number of retry attempts when disconnecting
const MAX_DISCONNECT_RETRIES: u8 = 3;
/// Delay between disconnect retry attempts in milliseconds
const DISCONNECT_RETRY_DELAY_MS: u64 = 100;
/// Maximum number of WiFi reconnection attempts
const WIFI_RECONNECT_ATTEMPTS: u8 = 3;
/// Delay between reconnection attempts in milliseconds
const WIFI_RECONNECT_DELAY_MS: u64 = 100;
/// Interval for checking WiFi connection status in milliseconds
const WIFI_CHECK_INTERVAL_MS: u64 = 50;
/// Maximum number of consecutive connection failures before giving up
const MAX_CONSECUTIVE_FAILURES: u8 = 2;

/// Static cell for network stack resources
static STACK_RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();

/// Static cell for WiFi controller
static WIFI_CONTROLLER: StaticCell<EspWifiController<'static>> = StaticCell::new();

#[derive(Debug)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Failed,
}

/// Errors that can occur during WiFi monitoring
#[derive(Debug, Error)]
pub enum MonitorError {
    /// Error during WiFi connection operations
    #[error("WiFi connection error")]
    Connection(#[from] WifiConnectionError),
}

/// Result of the WiFi monitoring task
#[derive(Debug)]
pub enum MonitorTaskResult {
    /// Task completed normally (connection stable)
    Success,
    /// Connection failed and could not be recovered
    ConnectionFailure,
    /// Task failed due to an internal error
    InternalError(MonitorError),
}

/// Error within WiFi connection
#[derive(Debug, Error)]
pub enum WifiConnectionError {
    /// Error when connecting to the Wifi
    #[error("Failed to connect to the Wifi.")]
    WifiConnectionFailed,

    /// Error during WiFi initialization
    #[error("Failed to initialize the Wifi.")]
    WifiInitialization(WifiInitializationError),

    /// Error during WiFi operation
    #[error("An error occured with the Wifi.")]
    Wifi(EspWifiError),

    /// Failed to spawn network task
    #[error("Failed to spawn network task")]
    NetworkTaskSpawnFailed,
}

#[derive(Debug, thiserror::Error)]
pub enum WifiDisconnectError {
    #[error("Failed to disconnect from WiFi")]
    Disconnect,

    #[error("WiFi disconnect verification failed after {attempts} attempts")]
    Verification { attempts: u8 },
}

impl From<WifiInitializationError> for WifiConnectionError {
    fn from(error: WifiInitializationError) -> Self {
        Self::WifiInitialization(error)
    }
}

impl From<EspWifiError> for WifiConnectionError {
    fn from(error: EspWifiError) -> Self {
        Self::Wifi(error)
    }
}

pub async fn connect_to_wifi<'a>(
    spawner: Spawner,
    timg0: TIMG0,
    wifi: WIFI,
    radio_clk: RADIO_CLK,
    rng: Rng,
    ssid: String<32>,
    password: String<64>,
) -> Result<(WifiController<'a>, Stack<'a>), WifiConnectionError> {
    info!("Connecting to WiFi");
    let timg0 = TimerGroup::new(timg0);

    let (mut controller, stack, runner) =
        match create_controller_and_stack(timg0, rng, wifi, radio_clk).await {
            Ok(tuple) => tuple,
            Err(_) => return Err(WifiConnectionError::WifiConnectionFailed),
        };

    if let Err(e) = spawner.spawn(wifi_management_task(runner)) {
        error!("Failed to spawn network task: {e:?}");
        return Err(WifiConnectionError::NetworkTaskSpawnFailed);
    }

    let mut attempts = 0;
    while attempts < WIFI_RECONNECT_ATTEMPTS {
        debug!("Connecting to network ...");
        let connect_result =
            connect_to_network(&mut controller, ssid.clone(), password.clone()).await;
        if connect_result.is_err() {
            let e = connect_result.err().unwrap();
            error!(
                "WiFi connection attempt {}/{} failed: {e:?}",
                attempts + 1,
                WIFI_RECONNECT_ATTEMPTS
            );
        } else {
            debug!("Wait for network link");
            loop {
                if stack.is_link_up() {
                    break;
                }
                Timer::after(Duration::from_millis(500)).await;
            }

            debug!("Wait for IP address");
            loop {
                if let Some(config) = stack.config_v4() {
                    info!("Connected to WiFi with IP address {}", config.address);
                    break;
                }
                Timer::after(Duration::from_millis(500)).await;
            }

            // Verify connection is stable
            Timer::after(Duration::from_millis(WIFI_RECONNECT_DELAY_MS)).await;
            match controller.is_connected() {
                Ok(true) => {
                    info!("WiFi connection established and stable");
                    return Ok((controller, stack));
                }
                Ok(false) => {
                    error!(
                        "WiFi connection attempt {}/{} failed. Failed to establish a stable connection.",
                        attempts + 1,
                        WIFI_RECONNECT_ATTEMPTS
                    );
                }
                Err(e) => {
                    error!(
                        "WiFi connection attempt {}/{} failed: {e:?}",
                        attempts + 1,
                        WIFI_RECONNECT_ATTEMPTS
                    );
                }
            }
        }

        attempts += 1;
        if attempts < WIFI_RECONNECT_ATTEMPTS {
            Timer::after(Duration::from_millis(WIFI_RECONNECT_DELAY_MS)).await;
        }
    }

    Err(WifiConnectionError::WifiConnectionFailed)
}

/// Connect to WiFi
async fn create_controller_and_stack<'a>(
    timg0: TimerGroup<TIMG0>,
    rng: Rng,
    wifi: WIFI,
    radio_clock_control: RADIO_CLK,
) -> Result<
    (
        WifiController<'a>,
        Stack<'a>,
        Runner<'a, WifiDevice<'a, WifiStaDevice>>,
    ),
    WifiConnectionError,
> {
    let mut rng_wrapper = RngWrapper::from(rng);
    let seed = rng_wrapper.next_u64();
    debug!("Use random seed 0x{seed:016x}");

    let wifi_controller = initialize_wifi(timg0.timer0, rng, radio_clock_control)?;
    let wifi_controller: &'static mut _ = WIFI_CONTROLLER.init(wifi_controller);

    let (wifi_interface, mut controller) =
        new_wifi_with_mode(wifi_controller, wifi, WifiStaDevice)?;
    controller.set_power_saving(PowerSaveMode::None)?;

    let config = Config::dhcpv4(DhcpConfig::default());

    debug!("Initialize network stack");
    let stack_resources: &'static mut _ = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = new_network_stack(wifi_interface, config, stack_resources, seed);

    Ok((controller, stack, runner))
}

/// Fallible task for WiFi connection
async fn connect_to_network(
    controller: &mut WifiController<'_>,
    ssid: String<32>,
    password: String<64>,
) -> Result<(), WifiConnectionError> {
    debug!("Start connection");
    debug!("Device capabilities: {:?}", controller.capabilities());

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = Configuration::Client(ClientConfiguration {
            ssid: ssid.clone(),
            password: password.clone(),
            ..Default::default()
        });
        controller.set_configuration(&client_config)?;
        debug!("Starting WiFi controller");

        controller.start_async().await?;
        debug!("WiFi controller started");
    }

    debug!("Connect to WiFi network");

    match controller.connect_async().await {
        Ok(_) => Ok(()),
        Err(e) => Err(WifiConnectionError::Wifi(e)),
    }
}

pub async fn disconnect_from_wifi(
    wifi_controller: &mut WifiController<'_>,
) -> Result<(), WifiDisconnectError> {
    let mut retries = 0;
    while retries < MAX_DISCONNECT_RETRIES {
        match wifi_controller.is_connected() {
            Ok(is_connected) => {
                if !is_connected {
                    info!("WiFi already disconnected");
                    return Ok(());
                }

                debug!("Disconnecting from Wifi ...");
                match wifi_controller.disconnect() {
                    Ok(_) => {
                        // Wait briefly to ensure disconnection completes
                        Timer::after(Duration::from_millis(DISCONNECT_RETRY_DELAY_MS)).await;

                        // Verify disconnection
                        match wifi_controller.is_connected() {
                            Ok(still_connected) => {
                                if !still_connected {
                                    info!("WiFi successfully disconnected");
                                    return Ok(());
                                }
                            }
                            Err(e) => match e {
                                EspWifiError::Disconnected => return Ok(()),
                                _ => error!(
                                    "Failed to disconnect WiFi (attempt {}/{}): {e:?}",
                                    retries + 1,
                                    MAX_DISCONNECT_RETRIES
                                ),
                            },
                        }
                    }
                    Err(e) => {
                        match e {
                            EspWifiError::Disconnected => return Ok(()),
                            _ => error!(
                                "Failed to disconnect WiFi (attempt {}/{}): {e:?}",
                                retries + 1,
                                MAX_DISCONNECT_RETRIES
                            ),
                        }

                        if retries == MAX_DISCONNECT_RETRIES - 1 {
                            return Err(WifiDisconnectError::Disconnect);
                        }
                    }
                }
            }
            Err(e) => match e {
                EspWifiError::Disconnected => return Ok(()),
                _ => error!("Failed to check WiFi connection status: {e:?}"),
            },
        }

        retries += 1;
        if retries < MAX_DISCONNECT_RETRIES {
            Timer::after(Duration::from_millis(DISCONNECT_RETRY_DELAY_MS)).await;
        }
    }

    Err(WifiDisconnectError::Verification {
        attempts: MAX_DISCONNECT_RETRIES,
    })
}

/// Monitors the WiFi connection and attempts to reconnect if needed
///
/// # Arguments
///
/// * `controller` - The WiFi controller to monitor
///
/// # Returns
///
/// Returns the current connection status or an error if monitoring fails
pub async fn monitor_connection(
    controller: &mut WifiController<'_>,
) -> Result<ConnectionStatus, MonitorError> {
    // Only check connection status, don't attempt recovery here
    match controller.is_connected() {
        Ok(true) => Ok(ConnectionStatus::Connected),
        Ok(false) => {
            error!("WiFi connection lost");
            Ok(ConnectionStatus::Disconnected)
        }
        Err(e) => {
            error!("Failed to check WiFi connection status: {:?}", e);
            Err(MonitorError::Connection(e.into()))
        }
    }
}

/// Task for ongoing network processing
#[embassy_executor::task]
async fn wifi_management_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    debug!("Starting wifi background runner ..");
    runner.run().await;
}

/// Monitors the WiFi connection and attempts to reconnect if needed
///
/// This task runs continuously, checking the connection status at regular intervals
/// and attempting to reconnect if the connection is lost. It communicates its status
/// through a channel to the main application.
///
/// # Arguments
///
/// * `controller` - The WiFi controller to monitor
/// * `ssid` - Network SSID
/// * `password` - Network password
/// * `status_sender` - Channel to send status updates to the main application
#[embassy_executor::task]
pub async fn wifi_monitor_task_with_channel(
    controller: &'static mut WifiController<'static>,
    status_sender: Sender<'static, CriticalSectionRawMutex, MonitorTaskResult, 1>,
) {
    debug!("Starting WiFi monitoring task");
    let mut consecutive_failures = 0;

    loop {
        match monitor_connection(controller).await {
            Ok(ConnectionStatus::Connected) => {
                debug!("WiFi connection is stable");
                consecutive_failures = 0;
            }
            Ok(status @ (ConnectionStatus::Disconnected | ConnectionStatus::Failed)) => {
                consecutive_failures += 1;
                error!(
                    "Connection failure detected ({}/{}): {:?}",
                    consecutive_failures, MAX_CONSECUTIVE_FAILURES, status
                );

                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    error!("Maximum failures reached, triggering device recovery");
                    if let Err(e) = status_sender.try_send(MonitorTaskResult::ConnectionFailure) {
                        error!("Failed to send connection failure status: {:?}", e);
                    }
                    break; // Exit immediately to allow device reset/sleep
                }
            }
            Err(e) => {
                error!(
                    "Critical monitor error, triggering immediate recovery: {:?}",
                    e
                );
                if let Err(send_err) = status_sender.try_send(MonitorTaskResult::InternalError(e)) {
                    error!("Failed to send error status: {:?}", send_err);
                }
                break; // Exit immediately on errors
            }
        }

        Timer::after(Duration::from_millis(WIFI_CHECK_INTERVAL_MS)).await;
    }

    error!("WiFi monitor task exiting to allow device recovery");
}
