// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

#![no_std]
#![no_main]

extern crate alloc;

use core::convert::Infallible;

use esp_hal::peripherals::Peripherals;
use esp_hal::ram;
use esp_hal::reset::software_reset;
use esp_hal::time::now;
use esp_hal_embassy::main;
use log::error;
use log::info;

use embassy_executor::Spawner;

use esp_alloc as _;

use esp_hal::clock::CpuClock;
use esp_hal::init as initialize_esp_hal;
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::Config as EspConfig;

use esp_hal_embassy::init as initialize_embassy;

use logging::send_logs_to_server;
use thiserror::Error;

use heapless::String;

use esp_backtrace as _;

mod board_components;

mod cell;
use self::cell::SyncUnsafeCell;

mod data_recording;
use self::data_recording::send_metrics_to_server;

mod device_meta;

mod logging;
use self::logging::setup_logger as setup_logging;

mod meta;

mod random;
use self::random::RngWrapper;

mod sensor;
use self::sensor::read_sensor_data;
use self::sensor::SensorPeripherals;

mod sensor_data;

mod sleep;
use self::sleep::enter_deep as enter_deep_sleep;

mod timing;
use self::timing::send_timing_data;

mod wifi;
use self::wifi::WifiConnectionError as WifiError;

/// Duration of deep sleep
const DEEP_SLEEP_DURATION_IN_SECONDS: u32 = 30;

/// SSID for WiFi network
const WIFI_SSID: &str = env!("WIFI_SSID");

/// Password for WiFi network
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Size of heap for dynamically-allocated memory
const HEAP_MEMORY_SIZE: usize = 72 * 1024;

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

    /// An error within WiFi operations
    #[error("An error within WiFi operations")]
    Wifi {
        #[from]
        source: WifiError,
    },
}

fn init_heap() {
    static mut HEAP: core::mem::MaybeUninit<[u8; HEAP_MEMORY_SIZE]> =
        core::mem::MaybeUninit::uninit();

    unsafe {
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            HEAP.as_mut_ptr() as *mut u8,
            HEAP_MEMORY_SIZE,
            esp_alloc::MemoryCapability::Internal.into(),
        ));
    }
}

/// Main task
#[main]
async fn main(spawner: Spawner) {
    let peripherals = initialize_esp_hal({
        let mut config = EspConfig::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    // SAFETY:
    // This is the only place where a mutable reference is taken
    let boot_count: Option<&'static mut _> = unsafe { BOOT_COUNT.get().as_mut() };
    // SAFETY:
    // This is pointing to a valid value
    let boot_count: &'static mut _ = unsafe { boot_count.unwrap_unchecked() };
    info!("Current boot count = {boot_count}");
    *boot_count += 1;

    let logger_result = setup_logging(*boot_count);
    if logger_result.is_err() {
        // Everything is stuffed. Just go back to sleep
        enter_deep_sleep(
            peripherals.LPWR,
            hifitime::Duration::from_seconds(DEEP_SLEEP_DURATION_IN_SECONDS as f64),
        );
    }

    main_fallible(spawner, peripherals, *boot_count).await;
}

/// Main task that can return an error
async fn main_fallible(spawner: Spawner, mut peripherals: Peripherals, boot_count: u32) -> ! {
    init_heap();

    let start_time = now();
    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    initialize_embassy(systimer.alarm0);

    let rng = Rng::new(&mut peripherals.RNG);

    // Connect to WiFi and get network stack
    let ssid_result = String::<32>::try_from(WIFI_SSID);
    let password_result = String::<64>::try_from(WIFI_PASSWORD);

    if ssid_result.is_err() || password_result.is_err() {
        error!("No valid Wifi SSID or password provided");
        enter_deep_sleep(
            peripherals.LPWR,
            hifitime::Duration::from_seconds(DEEP_SLEEP_DURATION_IN_SECONDS as f64),
        );
    } else {
        let ssid = ssid_result.unwrap();
        let password = password_result.unwrap();

        info!("Connecting to WiFi network");
        let wifi_connect_result = wifi::connect_to_wifi(
            spawner,
            peripherals.TIMG0,
            peripherals.WIFI,
            peripherals.RADIO_CLK,
            rng,
            ssid,
            password,
        )
        .await;

        if wifi_connect_result.is_err() {
            error!(
                "Failed to connect to WiFi: {:?}",
                wifi_connect_result.err().unwrap()
            );
        } else {
            let (mut wifi_controller, stack) = wifi_connect_result.unwrap();

            match send_logs_to_server(stack).await {
                Ok(_) => (),
                Err(e) => {
                    error!("Failed to send the logs to the server: {e:?}");
                }
            };

            // Get duration for operations
            let current_time = now();
            let wifi_start_time_in_micro_seconds = current_time
                .checked_duration_since(start_time)
                .unwrap()
                .to_micros();

            if let Err(e) = send_timing_data(stack, boot_count).await {
                error!("Failed to send timing data: {e:?}");
                // Continue execution even if timing data fails, as we can still try to send sensor data
            }
            match send_logs_to_server(stack).await {
                Ok(_) => (),
                Err(e) => {
                    error!("Failed to send the logs to the server: {e:?}");
                }
            };

            let sensor_read_result = read_sensor_data(SensorPeripherals {
                sda: peripherals.GPIO10,
                scl: peripherals.GPIO11,
                pressure_sensor_enable: peripherals.GPIO18,
                i2c0: peripherals.I2C0,
                rng,
            })
            .await;

            if sensor_read_result.is_err() {
                error!("Failed to send the logs to the server");
            } else {
                let (bme280_reading, ads1115_reading) = sensor_read_result.unwrap();

                match send_logs_to_server(stack).await {
                    Ok(_) => (),
                    Err(e) => {
                        error!("Failed to send the logs to the server: {e:?}");
                    }
                };

                let _ = send_metrics_to_server(
                    stack,
                    bme280_reading,
                    ads1115_reading,
                    boot_count,
                    start_time,
                    wifi_start_time_in_micro_seconds,
                )
                .await;
            }

            // Prepare to shut down. Turn off the logger
            info!(
                "Entering deep sleep for {}s",
                DEEP_SLEEP_DURATION_IN_SECONDS,
            );

            match send_logs_to_server(stack).await {
                Ok(_) => (),
                Err(e) => {
                    error!("Failed to send the logs to the server: {e:?}");
                }
            };

            // Ensure WiFi is disconnected properly before device state transition
            let wifi_disconnect_result = wifi::disconnect_from_wifi(&mut wifi_controller).await;
            match wifi_disconnect_result {
                Ok(_) => {
                    info!("WiFi disconnected successfully, entering deep sleep");
                    enter_deep_sleep(
                        peripherals.LPWR,
                        hifitime::Duration::from_seconds(DEEP_SLEEP_DURATION_IN_SECONDS as f64),
                    );
                }
                Err(e) => {
                    error!("Failed to disconnect WiFi, performing software reset: {e}");
                    software_reset();
                }
            }
        }
    }

    // This is unreachable as both deep_sleep and software_reset never return
    unreachable!("Device should have entered deep sleep or reset");
}
