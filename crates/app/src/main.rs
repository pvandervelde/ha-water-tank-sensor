// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

#![no_std]
#![no_main]

use core::convert::Infallible;

use embassy_net::Stack;
use esp_hal::peripherals::TIMG1;
use esp_hal::time::now;
use esp_hal::time::Instant;
use esp_wifi::wifi::WifiController;
use log::error;
use log::info;

use embassy_executor::Spawner;

use embassy_time::Timer;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::channel::Sender;

use esp_alloc::heap_allocator;

use esp_hal::clock::CpuClock;
use esp_hal::dma::DmaBufError;
use esp_hal::dma::DmaDescriptor;
use esp_hal::gpio::GpioPin;
use esp_hal::gpio::Input;
use esp_hal::gpio::Level;
use esp_hal::gpio::Output;
use esp_hal::gpio::Pull;
use esp_hal::i2c::master::Config as I2cConfig;
use esp_hal::i2c::master::I2c;
use esp_hal::init as initialize_esp_hal;
use esp_hal::peripherals::I2C0;
use esp_hal::peripherals::RADIO_CLK;
use esp_hal::peripherals::TIMG0;
use esp_hal::peripherals::WIFI;
use esp_hal::prelude::*; // RateExtU32, main, ram
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::timer::systimer::Target;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::Config as EspConfig;

use esp_hal_embassy::init as initialize_embassy;

use scopeguard::guard;
use scopeguard::ScopeGuard;
use thiserror::Error;

use heapless::String;

use esp_backtrace as _;

use static_cell::StaticCell;

mod cell;
use self::cell::SyncUnsafeCell;

mod clock;
use self::clock::Clock;
use self::clock::Error as ClockError;

mod data_recording;
use self::data_recording::update_task as send_data_task;

mod device_meta;

mod http;

mod logging;
use self::logging::setup as setup_logging;

mod meta;

mod random;
use self::random::RngWrapper;

mod sensor;
use self::sensor::read_environmental_data_task as sample_sensor_task;

mod sensor_data;
use sensor_data::Reading;

mod sleep;
use self::sleep::enter_deep as enter_deep_sleep;

mod wifi;
use self::wifi::Error as WifiError;

/// Duration of deep sleep
const DEEP_SLEEP_DURATION_IN_SECONDS: u32 = 30;

/// Period to wait after the data has been sent, before going to deep sleep
const WAIT_AFTER_SENT_PERIOD_IN_SECONDS: u64 = 5;

/// SSID for WiFi network
const WIFI_SSID: &str = env!("WIFI_SSID");

/// Password for WiFi network
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;

/// A channel between all the data processors and the main function. Used to let
/// the main function know when the work is done.
static DATA_SEND_CHANNEL: StaticCell<Channel<NoopRawMutex, bool, 3>> = StaticCell::new();

/// A channel between sensor sampler and data processor
static ENVIRONMENTAL_CHANNEL: StaticCell<Channel<NoopRawMutex, Reading, 3>> = StaticCell::new();

/// Stored boot count between deep sleep cycles
///
/// This is a statically allocated variable and it is placed in the RTC Fast
/// memory, which survives deep sleep.
#[ram(rtc_fast)]
static BOOT_COUNT: SyncUnsafeCell<u32> = SyncUnsafeCell::new(0);

/// An error
#[derive(Debug, Error)]
enum Error {
    /// An impossible error existing only to satisfy the type system
    #[error("An impossible error existing only to satisfy the type system")]
    Impossible {
        #[from]
        source: Infallible,
    },

    /// Error while parsing SSID or password
    #[error("Error while parsing SSID or password")]
    ParseCredentials,

    /// An error within WiFi operations
    #[error("An error within WiFi operations")]
    Wifi {
        #[from]
        source: WifiError,
    },

    /// An error within clock operations
    #[error("An error within clock operations")]
    Clock {
        #[from]
        source: ClockError,
    },
}

async fn connect_to_wifi<'a>(
    spawner: Spawner,
    timg0: TIMG0,
    wifi: WIFI,
    radio_clk: RADIO_CLK,
    rng: Rng,
) -> Result<
    (
        ScopeGuard<WifiController<'a>, impl FnOnce(WifiController<'a>)>,
        Stack<'a>,
    ),
    Error,
> {
    let ssid = String::<32>::try_from(WIFI_SSID).map_err(|()| Error::ParseCredentials)?;
    let password = String::<64>::try_from(WIFI_PASSWORD).map_err(|()| Error::ParseCredentials)?;

    info!("Connect to WiFi");
    let timg0 = TimerGroup::new(timg0);
    let (controller, stack) =
        wifi::connect(spawner, timg0, rng, wifi, radio_clk, (ssid, password)).await?;
    let guard = guard(controller, |mut c| {
        info!("Disconnecting from wifi ...");
        let _ = c.disconnect();
    });

    Ok((guard, stack))
}

/// Load clock from RTC memory of from server
async fn load_clock<'a>(stack: Stack<'_>) -> Result<Clock, Error> {
    let clock = if let Some(clock) = Clock::from_rtc_memory() {
        info!("Clock loaded from RTC memory");
        clock
    } else {
        info!("Synchronize clock from server");

        Clock::from_server(stack).await?
    };

    Ok(clock)
}

/// Main task
#[main]
async fn main(spawner: Spawner) {
    setup_logging();

    // SAFETY:
    // This is the only place where a mutable reference is taken
    let boot_count: Option<&'static mut _> = unsafe { BOOT_COUNT.get().as_mut() };
    // SAFETY:
    // This is pointing to a valid value
    let boot_count: &'static mut _ = unsafe { boot_count.unwrap_unchecked() };
    info!("Current boot count = {boot_count}");
    *boot_count += 1;

    if let Err(error) = main_fallible(spawner, *boot_count).await {
        error!("Error while running firmware: {error:?}");
    }
}

/// Main task that can return an error
async fn main_fallible(spawner: Spawner, boot_count: u32) -> Result<(), Error> {
    let peripherals = initialize_esp_hal({
        let mut config = EspConfig::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    heap_allocator!(HEAP_MEMORY_SIZE);

    let start_time = now();

    // Start the wifi
    {
        let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
        initialize_embassy(systimer.alarm0);

        let rng = Rng::new(peripherals.RNG);

        let (mut wifi_guard, stack) = connect_to_wifi(
            spawner,
            peripherals.TIMG0,
            peripherals.WIFI,
            peripherals.RADIO_CLK,
            rng,
        )
        .await?;

        let rng_wrapper = RngWrapper::from(rng);

        // Turn on the power for the pressure sensor
        // Record time

        let clock = load_clock(stack).await?;
        info!("Now is {}", clock.now());

        let data_sent_channel: &'static mut _ = DATA_SEND_CHANNEL.init(Channel::new());
        let data_sent_receiver = data_sent_channel.receiver();
        let data_sent_sender = data_sent_channel.sender();

        info!("Setup data sending task");
        let environmental_data_sender = setup_data_transmitting_task(
            spawner,
            stack,
            rng_wrapper,
            data_sent_sender,
            boot_count,
            start_time,
        )?;

        // Number of samples

        info!("Setup sensor task");
        setup_sensor_task(
            spawner,
            SensorPeripherals {
                sda: peripherals.GPIO10,
                scl: peripherals.GPIO11,
                i2c0: peripherals.I2C0,
                rng,
            },
            clock.clone(),
            environmental_data_sender,
        );

        // Turn on the pressure sensor
        enable_pressure_sensor();

        // Wait for the pressure sensor to be stable

        // Wait at least x milliseconds
        // Then check the voltage on the pressure sensor

        // Take pressure sensor readings
        read_pressure_sensor().await;

        // Turn off power to the pressure sensor when we're done taking the recordings
        disable_pressure_sensor();

        info!("Waiting for sensors to complete tasks");
        let was_processed = data_sent_receiver.receive().await;
        if !was_processed {
            error!("Failed to process the data");
        }

        info!("Wait for {}s", WAIT_AFTER_SENT_PERIOD_IN_SECONDS);
        Timer::after(embassy_time::Duration::from_secs(
            WAIT_AFTER_SENT_PERIOD_IN_SECONDS,
        ))
        .await;

        // Shut down sensors

        // Shut down processing

        info!("Saving time to RTC memory ...");
        clock.save_to_rtc_memory(hifitime::Duration::from_seconds(
            DEEP_SLEEP_DURATION_IN_SECONDS as f64,
        ));

        // If something goes wrong before this point then the guard is dropped which causes
        // the wifi to disconnect. If that
        info!("Checking wifi status ...");
        let connected_result = wifi_guard.is_connected();
        if connected_result.is_ok() && connected_result.unwrap() {
            info!("Disconnecting from wifi ...");
            let _ = wifi_guard.disconnect();
        }
    }

    info!(
        "Entering deep sleep for {}s",
        DEEP_SLEEP_DURATION_IN_SECONDS,
    );
    enter_deep_sleep(
        peripherals.LPWR,
        hifitime::Duration::from_seconds(DEEP_SLEEP_DURATION_IN_SECONDS as f64),
    );
}

fn setup_data_transmitting_task(
    spawner: Spawner,
    stack: Stack<'static>,
    rng_wrapper: RngWrapper,
    data_sent_sender: Sender<'static, NoopRawMutex, bool, 3>,
    boot_count: u32,
    system_start_time: Instant,
) -> Result<Sender<'static, NoopRawMutex, Reading, 3>, Error> {
    info!("Create channel");
    let channel: &'static mut _ = ENVIRONMENTAL_CHANNEL.init(Channel::new());
    let receiver = channel.receiver();
    let sender = channel.sender();

    info!("Spawning data sending task");
    spawner.must_spawn(send_data_task(
        stack,
        rng_wrapper,
        receiver,
        data_sent_sender,
        boot_count,
        system_start_time,
    ));

    Ok(sender)
}

/// Peripherals used by the sensor
struct SensorPeripherals {
    /// I²C SDA pin
    sda: GpioPin<10>,
    /// I²C SCL pin
    scl: GpioPin<11>,

    /// I²C interface
    i2c0: I2C0,

    /// Random number generator
    rng: Rng,
}

/// Setup sensor task
fn setup_sensor_task(
    spawner: Spawner,
    peripherals: SensorPeripherals,
    clock: Clock,
    sender: Sender<'static, NoopRawMutex, Reading, 3>,
) {
    info!("Create I²C bus");
    let i2c_config = I2cConfig {
        frequency: 25_u32.kHz(),
        ..Default::default()
    };
    let i2c = I2c::new(peripherals.i2c0, i2c_config)
        .with_sda(peripherals.sda)
        .with_scl(peripherals.scl)
        .into_async();

    info!("Spawning environmental sensor task");
    spawner.must_spawn(sample_sensor_task(i2c, peripherals.rng, sender, clock));
}
