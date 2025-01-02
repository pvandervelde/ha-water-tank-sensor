#![no_std]
#![no_main]

extern crate alloc;
extern crate scopeguard;

use embedded_io::*;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    prelude::*,
    reset,
    rng::Rng,
    time::{self, Duration},
    timer::timg::TimerGroup,
};
use log::{debug, error, info};
use smoltcp::{
    iface::{SocketSet, SocketStorage},
    wire::{DhcpOption, IpAddress, Ipv4Address},
};
use thiserror::Error;

mod logging;
use logging::setup as setup_logging;

mod wifi;
use wifi::{connect_to_wifi, initialize_wifi};

// Application flow
// - Startup. We have just woken up from deep sleep, or have been rebooted.
// - Capture the current time. This is used later to determine how long the processing has taken.
//   Used to determine the length of deep sleep time. If the current time is unknown then we
//   connect to a well known time signal and store it for later.
// - Start up the submersible sensor and get a number of pressrue readings
//   We average a number of readings to ensure that we have something reliable
// - Get the environment readings. Again we get multiple readings and then average those
// - Join the Wifi
// - Transmit the data and the internal logs / metrics
// - Disconnect from the Wifi
// - Go into deep sleep for the rest of the interval

// Physical pin connections
// -----------------------------------
// Pin 11/12 (P?, P?) = i2c bus. (P? = SDA, P? = SCL)
//

// CONSTANTS

/// Period to wait between readings
const SAMPLING_PERIOD: Duration = Duration::secs(60);

/// Duration of deep sleep
const DEEP_SLEEP_DURATION: Duration = Duration::secs(300);

/// Period to wait before going to deep sleep
const AWAKE_PERIOD: Duration = Duration::secs(300);

/// SSID for WiFi network
const WIFI_SSID: &str = env!("WIFI_SSID");

/// Password for WiFi network
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;

/// Buffer for SPI DMA
//static BUFFER: StaticCell<[u8; BUFFERS_SIZE]> = StaticCell::new();

/// RX Buffer for SPI DMA
//static RX_BUFFER: StaticCell<[u8; BUFFERS_SIZE]> = StaticCell::new();

#[derive(Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// Indicates that we failed to connect to the provided Wifi channel.
    #[error("Failed to connect to the provided Wifi SSID channel.")]
    FailedToConnectToWifiChannel,

    /// Indicates that we couldn't initialize the onboard wifi device.
    #[error("Failed to initialize the onboard Wifi device.")]
    FailedToInitializeWifiDevice,

    /// Indicats that the wifi password that was provided isn't a valid string.
    #[error("The provided Wifi password is not a valid string.")]
    InvalidWifiPassword,

    /// Indicats that the wifi SSID that was provided isn't a valid string.
    #[error("The provided Wifi SSID is not a valid string.")]
    InvalidWifiSSID,
}

#[entry]
fn main() -> ! {
    setup_logging();

    if let Err(error) = main_fallible() {
        error!("Error while running firmware: {error:?}");
    }

    let deadline = time::now() + Duration::secs(10);
    let delay = Delay::new();
    while time::now() < deadline {
        // Reset the device here because we are in an error state
        let diff = deadline - time::now();
        info!("Resetting device in: {} seconds", diff.to_secs());
        delay.delay_millis(1000);
    }

    reset::software_reset();

    loop {
        // Trick the compiler into thinking we're still here ...
    }
}

/// The main function that returns an error if anything goes wrong.
fn main_fallible() -> Result<(), Error> {
    let peripherals = esp_hal::init({
        let mut config = esp_hal::Config::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    heap_allocator!(HEAP_MEMORY_SIZE);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);
    let random_value = rng.random();

    // let clock = load_clock(
    //     spawner,
    //     peripherals.TIMG0,
    //     peripherals.WIFI,
    //     peripherals.RADIO_CLK,
    //     rng,
    // )?;

    // info!("Now is {}", clock.now()?);

    //
    // Connect to the Wifi
    //

    let esp_wifi_controller_result = initialize_wifi(timg0, rng, peripherals.RADIO_CLK);
    if esp_wifi_controller_result.is_err() {
        return Err(Error::FailedToInitializeWifiDevice);
    }

    let esp_wifi_controller = esp_wifi_controller_result.unwrap();

    // Create the wifi socket over which communication is run
    let mut socket_set_entries: [SocketStorage; 3] = Default::default();
    let mut socket_set = SocketSet::new(&mut socket_set_entries[..]);
    let mut dhcp_socket = smoltcp::socket::dhcpv4::Socket::new();

    // we can set a hostname here (or add other DHCP options. See: https://en.wikipedia.org/wiki/Dynamic_Host_Configuration_Protocol#Options)
    dhcp_socket.set_outgoing_options(&[DhcpOption {
        kind: 12, // DHCP option 12 sets the hostname
        data: b"tank-monitor",
    }]);
    socket_set.add(dhcp_socket);

    let ssid_conversion = WIFI_SSID.try_into();
    if ssid_conversion.is_err() {
        return Err(Error::InvalidWifiSSID);
    }

    let password_conversion = WIFI_PASSWORD.try_into();
    if password_conversion.is_err() {
        return Err(Error::InvalidWifiPassword);
    }

    let wifi_connection_result = connect_to_wifi(
        &esp_wifi_controller,
        socket_set,
        random_value,
        peripherals.WIFI,
        (ssid_conversion.unwrap(), password_conversion.unwrap()),
    );
    if wifi_connection_result.is_err() {
        // If this fails we check the error:
        // - If invalid password / username then we just give up
        // - If other error then we go to sleep and try again in X time

        return Err(Error::FailedToConnectToWifiChannel);
    }

    let mut controllers = wifi_connection_result.unwrap();

    debug!("Start busy loop on main");

    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];
    let mut socket = controllers
        .network_mut()
        .get_socket(&mut rx_buffer, &mut tx_buffer);

    loop {
        debug!("Making HTTP request");
        socket.work();

        socket
            .open(IpAddress::Ipv4(Ipv4Address::new(142, 250, 185, 115)), 80)
            .unwrap();

        socket
            .write(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
            .unwrap();
        socket.flush().unwrap();

        let deadline = time::now() + Duration::secs(20);
        let mut buffer = [0u8; 512];
        while let Ok(len) = socket.read(&mut buffer) {
            let to_print = unsafe { core::str::from_utf8_unchecked(&buffer[..len]) };
            debug!("{}", to_print);

            if time::now() > deadline {
                debug!("Timeout");
                break;
            }
        }
        debug!("");

        socket.disconnect();

        let deadline = time::now() + Duration::secs(5);
        while time::now() < deadline {
            socket.work();
        }
    }

    // Capture the current time from persistent memory. If the time isn't in persistent memory
    // then grab it from the internet

    // Start the submersible sensor and get a number of pressure readings

    // Get environment readings (multiple)

    // Transmit data

    // Disconnect from the wifi

    // Deep sleep

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/v0.22.0/examples/src/bin
}
