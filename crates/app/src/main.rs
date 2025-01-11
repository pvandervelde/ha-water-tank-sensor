// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

#![no_std]
#![no_main]

use core::convert::Infallible;

use embassy_net::Stack;
use esp_wifi::wifi::WifiController;
use log::error;
use log::info;

use embassy_executor::Spawner;

use embassy_time::Duration;
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
use time::OffsetDateTime;

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

mod http;
use self::http::Client as HttpClient;

mod logging;
use self::logging::setup as setup_logging;

mod random;
use self::random::RngWrapper;

mod sensor;
use self::sensor::sample_task as sample_sensor_task;

mod sensor_data;
use sensor_data::Reading;
use sensor_data::Sample;

mod sleep;
use self::sleep::enter_deep as enter_deep_sleep;

mod wifi;
use self::wifi::Error as WifiError;

mod worldtimeapi;

/// Period to wait between readings
const SAMPLING_PERIOD: Duration = Duration::from_secs(10);

/// Duration of deep sleep
const DEEP_SLEEP_DURATION: Duration = Duration::from_secs(30);

/// Period to wait before going to deep sleep
const AWAKE_PERIOD: Duration = Duration::from_secs(30);

/// SSID for WiFi network
const WIFI_SSID: &str = env!("WIFI_SSID");

/// Password for WiFi network
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;

/// A channel between sensor sampler and display updater
static CHANNEL: StaticCell<Channel<NoopRawMutex, Reading, 3>> = StaticCell::new();

/// Size of SPI DMA descriptors
const DESCRIPTORS_SIZE: usize = 8 * 3;

/// Descriptors for SPI DMA
static DESCRIPTORS: StaticCell<[DmaDescriptor; DESCRIPTORS_SIZE]> = StaticCell::new();

/// RX descriptors for SPI DMA
static RX_DESCRIPTORS: StaticCell<[DmaDescriptor; DESCRIPTORS_SIZE]> = StaticCell::new();

/// Size of SPI DMA buffers
const BUFFERS_SIZE: usize = 8 * 3;

/// Buffer for SPI DMA
static BUFFER: StaticCell<[u8; BUFFERS_SIZE]> = StaticCell::new();

/// RX Buffer for SPI DMA
static RX_BUFFER: StaticCell<[u8; BUFFERS_SIZE]> = StaticCell::new();

/// Stored boot count between deep sleep cycles
///
/// This is a statically allocated variable and it is placed in the RTC Fast
/// memory, which survives deep sleep.
#[ram(rtc_fast)]
static BOOT_COUNT: SyncUnsafeCell<u32> = SyncUnsafeCell::new(0);

/// An error
#[derive(Debug)]
enum Error {
    /// An impossible error existing only to satisfy the type system
    Impossible(Infallible),

    /// Error while parsing SSID or password
    ParseCredentials,

    /// An error within WiFi operations
    #[expect(unused, reason = "Never read directly")]
    Wifi(WifiError),

    /// An error within clock operations
    #[expect(unused, reason = "Never read directly")]
    Clock(ClockError),

    /// An error within creation of DMA buffers
    #[expect(unused, reason = "Never read directly")]
    DmaBuffer(DmaBufError),
}

impl From<Infallible> for Error {
    fn from(error: Infallible) -> Self {
        Self::Impossible(error)
    }
}

impl From<WifiError> for Error {
    fn from(error: WifiError) -> Self {
        Self::Wifi(error)
    }
}

impl From<ClockError> for Error {
    fn from(error: ClockError) -> Self {
        Self::Clock(error)
    }
}

impl From<DmaBufError> for Error {
    fn from(error: DmaBufError) -> Self {
        Self::DmaBuffer(error)
    }
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
        c.disconnect();
    });

    Ok((guard, stack))
}

/// Load clock from RTC memory of from server
async fn load_clock(spawner: Spawner, client: &mut HttpClient) -> Result<Clock, Error> {
    let clock = if let Some(clock) = Clock::from_rtc_memory() {
        info!("Clock loaded from RTC memory");
        clock
    } else {
        info!("Synchronize clock from server");

        let clock = Clock::from_server(client).await?;

        clock
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

    if let Err(error) = main_fallible(spawner).await {
        error!("Error while running firmware: {error:?}");
    }
}

/// Main task that can return an error
async fn main_fallible(spawner: Spawner) -> Result<(), Error> {
    let peripherals = initialize_esp_hal({
        let mut config = EspConfig::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    heap_allocator!(HEAP_MEMORY_SIZE);

    // Start the wifi
    {
        let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
        initialize_embassy(systimer.alarm0);

        let rng = Rng::new(peripherals.RNG);

        let (wifi_guard, stack) = connect_to_wifi(
            spawner,
            peripherals.TIMG0,
            peripherals.WIFI,
            peripherals.RADIO_CLK,
            rng,
        )
        .await?;

        let mut http_client = HttpClient::new(stack, RngWrapper::from(rng));

        let clock = load_clock(spawner, &mut http_client).await?;
        info!("Now is {}", clock.now()?);

        info!("Setup data sending task");
        let sender = setup_data_transmitting_task(spawner)?;

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
            sender,
        );

        info!("Stay awake for {}s", AWAKE_PERIOD.as_secs());
        Timer::after(AWAKE_PERIOD).await;

        clock.save_to_rtc_memory(DEEP_SLEEP_DURATION);

        // wifi guard goes out of scope here so it should shut down
    }

    enter_deep_sleep(peripherals.LPWR, DEEP_SLEEP_DURATION.into());
}

fn setup_data_transmitting_task(
    spawner: Spawner,
) -> Result<Sender<'static, NoopRawMutex, (OffsetDateTime, Sample), 3>, Error> {
    info!("Create channel");
    let channel: &'static mut _ = CHANNEL.init(Channel::new());
    let receiver = channel.receiver();
    let sender = channel.sender();

    info!("Spawn tasks");
    spawner.must_spawn(send_data_task(receiver));

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
    sender: Sender<'static, NoopRawMutex, (OffsetDateTime, Sample), 3>,
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

    spawner.must_spawn(sample_sensor_task(
        i2c,
        peripherals.rng,
        sender,
        clock,
        SAMPLING_PERIOD,
    ));
}
