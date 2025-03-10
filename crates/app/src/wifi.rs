// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Functions and task for WiFi connection

use log::debug;
use log::error;
use log::info;

use embassy_executor::Spawner;

use embassy_net::new as new_network_stack;

use esp_wifi::init as initialize_wifi;
use esp_wifi::wifi::new_with_mode as new_wifi_with_mode;
use esp_wifi::wifi::wifi_state;
use esp_wifi::wifi::ClientConfiguration;
use esp_wifi::wifi::Configuration;
use esp_wifi::wifi::WifiController;
use esp_wifi::wifi::WifiDevice;
use esp_wifi::wifi::WifiError as EspWifiError;
use esp_wifi::wifi::WifiEvent;
use esp_wifi::wifi::WifiStaDevice;
use esp_wifi::wifi::WifiState;
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

/// Static cell for network stack resources
static STACK_RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();

/// Static cell for WiFi controller
static WIFI_CONTROLLER: StaticCell<EspWifiController<'static>> = StaticCell::new();

/// Error within WiFi connection
#[derive(Debug, Error)]
pub enum Error {
    /// Error during WiFi initialization
    #[error("Failed to initialize the Wifi.")]
    WifiInitialization(WifiInitializationError),

    /// Error during WiFi operation
    #[error("An error occured with the Wifi.")]
    Wifi(EspWifiError),
}

impl From<WifiInitializationError> for Error {
    fn from(error: WifiInitializationError) -> Self {
        Self::WifiInitialization(error)
    }
}

impl From<EspWifiError> for Error {
    fn from(error: EspWifiError) -> Self {
        Self::Wifi(error)
    }
}

/// Connect to WiFi
pub async fn connect<'a>(
    spawner: Spawner,
    timg0: TimerGroup<TIMG0>,
    rng: Rng,
    wifi: WIFI,
    radio_clock_control: RADIO_CLK,
    (ssid, password): (String<32>, String<64>),
) -> Result<(WifiController<'a>, Stack<'a>), Error> {
    let mut rng_wrapper = RngWrapper::from(rng);
    let seed = rng_wrapper.next_u64();
    debug!("Use random seed 0x{seed:016x}");

    let wifi_controller = initialize_wifi(timg0.timer0, rng, radio_clock_control)?;
    let wifi_controller: &'static mut _ = WIFI_CONTROLLER.init(wifi_controller);

    let (wifi_interface, mut controller) =
        new_wifi_with_mode(wifi_controller, wifi, WifiStaDevice)?;

    let config = Config::dhcpv4(DhcpConfig::default());

    debug!("Initialize network stack");
    let stack_resources: &'static mut _ = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = new_network_stack(wifi_interface, config, stack_resources, seed);

    connection_fallible(&mut controller, ssid, password).await?;
    spawner.must_spawn(net_task(runner));

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

    Ok((controller, stack))
}

/// Fallible task for WiFi connection
async fn connection_fallible(
    controller: &mut WifiController<'_>,
    ssid: String<32>,
    password: String<64>,
) -> Result<(), Error> {
    debug!("Start connection");
    debug!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if wifi_state() == WifiState::StaConnected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await;
        }

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
            Ok(()) => {
                info!("Connected to WiFi network");
                break;
            }
            Err(error) => {
                error!("Failed to connect to WiFi network: {error:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }

    info!("Leave connection task");
    Ok(())
}

/// Task for ongoing network processing
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await;
}
