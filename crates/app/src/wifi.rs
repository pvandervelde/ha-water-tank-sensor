use blocking_network_stack::Stack;
use esp_hal::{
    delay::Delay,
    peripherals::{RADIO_CLK, TIMG0, WIFI},
    rng::Rng,
    time::{self, Duration},
    timer::timg::TimerGroup,
    Blocking,
};
use esp_wifi::wifi::{WifiController, WifiDevice, WifiStaDevice};
use esp_wifi::{
    init,
    wifi::{ClientConfiguration, Configuration},
};
use esp_wifi::{wifi::utils::create_network_interface, EspWifiController};
use heapless::String;
use log::{debug, error, info};
use scopeguard::{guard, ScopeGuard};
use smoltcp::iface::SocketSet;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum WifiError {
    // Indicates that we failed to connect to the given Wifi SSID
    #[error("Failed to connect to the given Wifi SSID.")]
    FailedToConnect { ssid: String<32> },

    /// Indicats that we failed to initialize the Wifi device
    #[error("Failed to initialize the Wifi device.")]
    FailedToInitializeTheWifiDevice,

    /// Indicates that there has been an internal error during the setup of the Wifi.
    #[error("An internal error occurred during the setup of the Wifi.")]
    InternalError,
}

pub struct ConnectionControllers<'a> {
    wifi: WifiController<'a>,
    network: Stack<'a, WifiDevice<'a, WifiStaDevice>>,
}

impl<'a> ConnectionControllers<'a> {
    pub fn network(&self) -> &Stack<'a, WifiDevice<'a, WifiStaDevice>> {
        &self.network
    }

    pub fn network_mut(&mut self) -> &mut Stack<'a, WifiDevice<'a, WifiStaDevice>> {
        &mut self.network
    }

    pub fn new(
        wifi: WifiController<'a>,
        network: Stack<'a, WifiDevice<'a, WifiStaDevice>>,
    ) -> Self {
        Self { wifi, network }
    }

    pub fn wifi(&self) -> &WifiController<'a> {
        &self.wifi
    }

    pub fn wifi_mut(&mut self) -> &mut WifiController<'a> {
        &mut self.wifi
    }
}

/// Connects to a nearby Wifi channel given a Wifi SSID and password. Returns the [WifiController]
/// that maintains the connection and the network [Stack] that is used for the communication.
///
/// ## Parameters
///
/// * 'wifi_controller' - A reference to the [EspWifiController] for the current device
/// * 'socket_set' - The [SocketSet] that is used to create the communication socket
/// * 'random' - A random number
/// * 'wifi' - The wifi peripheral
/// * 'ssid' - The SSID of the Wifi channel to connect to
/// * 'password' - The password for the Wifi channel
///
/// ## Errors
///
/// * [WifiError::InternalError]
/// * [WifiError::]
pub fn connect_to_wifi<'a>(
    wifi_controller: &'a esp_wifi::EspWifiController,
    socket_set: SocketSet<'a>,
    random: u32,
    wifi: WIFI,
    (ssid, password): (String<32>, String<64>),
) -> Result<ScopeGuard<ConnectionControllers<'a>, impl FnOnce(ConnectionControllers<'a>)>, WifiError>
{
    let new_wifi_result = create_network_interface(wifi_controller, wifi, WifiStaDevice);
    if new_wifi_result.is_err() {
        error!("Failed to initialize the ESP32 network interface");
        // Based on the code for 'create_network_interface' we should never get here. The code will
        // panic if the wrong configuration is provided. And this configuration is hard-coded in the esp-rs crate to be
        // the default config. So we should never get here.
        return Err(WifiError::InternalError);
    }

    let (iface, device, controller) = new_wifi_result.unwrap();

    let now = || time::now().duration_since_epoch().to_millis();
    let stack = Stack::new(iface, device, socket_set, now, random);

    let connection_controllers = ConnectionControllers::new(controller, stack);
    let mut connection_controllers_guarded = guard(connection_controllers, |mut c| {
        info!("Disconnecting from the Wifi ...");

        // We don't care about any errors but we can't use ? because the closure doesn't return anything
        let _ = c.wifi_mut().disconnect();
    });

    let client_config = Configuration::Client(ClientConfiguration {
        ssid: ssid.clone(),
        password,
        ..Default::default()
    });
    let res = connection_controllers_guarded
        .wifi_mut()
        .set_configuration(&client_config);
    debug!("wifi_set_configuration returned {:?}", res);

    let start_result = connection_controllers_guarded.wifi_mut().start();
    debug!("wifi start result: {:?}", start_result);

    let delay = Delay::new();
    let deadline = time::now() + Duration::secs(30);
    debug!("waiting for wifi device to start ...");
    loop {
        match connection_controllers_guarded.wifi().is_started() {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => {
                error!("{:?}", err);
            }
        }

        delay.delay_millis(100);
        if time::now() > deadline {
            return Err(WifiError::FailedToConnect { ssid: ssid.clone() });
        }
    }

    let connect_result = connection_controllers_guarded.wifi_mut().connect();
    debug!("wifi_connect {:?}", connect_result);

    // wait to get connected
    let deadline = time::now() + Duration::secs(30);
    info!("Connecting to {:?} ...", ssid.clone());
    loop {
        match connection_controllers_guarded.wifi().is_connected() {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => {
                error!("{:?}", err);
            }
        }

        delay.delay_millis(100);
        if time::now() > deadline {
            return Err(WifiError::FailedToConnect { ssid: ssid.clone() });
        }
    }

    // wait for getting an ip address
    info!("Waiting for an IP address ...");
    loop {
        connection_controllers_guarded.network().work();

        if connection_controllers_guarded.network().is_iface_up() {
            info!(
                "Obtained IP address: {:?}",
                connection_controllers_guarded.network().get_ip_info()
            );
            break;
        }
    }

    Ok(connection_controllers_guarded)
}

/// Initializes the ESP32 wifi unit.
///
/// ## Parameters
///
/// * 'timg0' - The [TimerGroup] that is connected to [TIMG0]
/// * 'rng' - The random number generator
/// * 'radio_clock_control' - The clock controller for the Wifi radio
///
/// # Errors
///
/// * [WifiError::FailedToInitializeTheWifiDevice] - Returns when the Wifi device could not be initialized.
pub fn initialize_wifi(
    timg0: TimerGroup<'_, TIMG0, Blocking>,
    rng: Rng,
    radio_clock_control: RADIO_CLK,
) -> Result<EspWifiController<'_>, WifiError> {
    let wifi_controller_result = init(timg0.timer0, rng, radio_clock_control);
    if wifi_controller_result.is_err() {
        return Err(WifiError::FailedToInitializeTheWifiDevice);
    }

    let wifi_controller = wifi_controller_result.unwrap();
    Ok(wifi_controller)
}
