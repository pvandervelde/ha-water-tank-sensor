// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Task for reading sensor value

// ESP32
use esp_hal::gpio::Output;
use esp_hal::gpio::{GpioPin, Level};
use esp_hal::i2c::master::Config as I2cConfig;
use esp_hal::i2c::master::Error as I2cError;
use esp_hal::i2c::master::I2c;
use esp_hal::peripherals::I2C0;
use esp_hal::prelude::nb::block;
use esp_hal::prelude::*; // RateExtU32, main, ram
use esp_hal::rng::Rng;
use esp_hal::Async;

// Components
use ads1x1x::channel;
use ads1x1x::ic::{Ads1115, Resolution16Bit};
use ads1x1x::Ads1x1x;
use ads1x1x::TargetAddr;

use bme280_rs::AsyncBme280;
use bme280_rs::Configuration;
use bme280_rs::Oversampling;
use bme280_rs::Sample as Bme280Sample;
use bme280_rs::SensorMode;

use heapless::Vec;

use libm::fabsf;

use log::debug;
use log::error;
use log::info;
use log::warn;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Sender;
use embassy_time::Delay;
use embassy_time::Timer;

use uom::si::electric_potential::volt;
use uom::si::f32::ElectricPotential as Voltage;
use uom::si::f32::Length;
use uom::si::f32::Pressure;
use uom::si::f32::Ratio;
use uom::si::f32::ThermodynamicTemperature as Temperature;
use uom::si::length::meter;
use uom::si::pressure::{self, hectopascal};
use uom::si::ratio::percent;
use uom::si::thermodynamic_temperature::degree_celsius;

use thiserror::Error;

use crate::board_components::{
    MPU_OUTPUT_VOLTAGE, PRESSURE_SENSOR_MAXIMUM_HEIGHT,
    PRESSURE_SENSOR_OUTPUT_RESISTOR_AFTER_PROBE, VOLTAGE_DIVIDER_BATTERY_RESISTOR_AFTER_PROBE,
    VOLTAGE_DIVIDER_BATTERY_RESISTOR_BEFORE_PROBE,
    VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_AFTER_PROBE,
    VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_BEFORE_PROBE,
};
use crate::sensor_data::Ads1115Data;
use crate::sensor_data::Bme280Data;
use crate::sensor_data::Error as DomainError;
use crate::sensor_data::NUMBER_OF_SAMPLES;
use crate::sensor_data::TIME_BETWEEN_SAMPLES_IN_SECONDS;

type Adc<'a> = Ads1x1x<I2c<'a, Async>, Ads1115, Resolution16Bit, ads1x1x::mode::OneShot>;

/// Interval to wait for sensor warmup, 10 milliseconds (aka 0.01 seconds)
const WARMUP_INTERVAL_IN_MILLISECONDS: f64 = 10.0;

// Interval to wait between checking if the pressure sensor voltage is stable
const PRESSURE_SENSOR_VOLTAGE_STABILIZATION_CHECK_INTERVAL_IN_SECONDS: f64 = 0.010;

// The voltage for the pressure sensor
const EXPECTED_PRESSURE_SENSOR_VOLTAGE: f32 = 24.0;

/// Error within sensor sampling
#[derive(Debug, Error)]
enum SensorError {
    // /// Error from clock
    // #[error("There was an error from the clock")]
    // Clock(#[expect(unused, reason = "Never read directly")] ClockError),
    /// Error from domain
    #[error("There was an error from the domain")]
    Domain(DomainError),

    /// Error from I²C bus
    #[error("An error has occurred with the I2c bus.")]
    I2c(#[expect(unused, reason = "Never read directly")] I2cError),

    #[error("The ADC voltage range could not be set.")]
    FailedToSetAdcRange,

    #[error("The voltage was too high.")]
    VoltageTooHigh,

    #[error("The voltage was too low.")]
    VoltageTooLow,

    #[error("The pressure sensor voltage is not stable.")]
    PressureSensorVoltageNotStable,
}

// impl From<ClockError> for SensorError {
//     fn from(error: ClockError) -> Self {
//         Self::Clock(error)
//     }
// }

impl From<DomainError> for SensorError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl From<I2cError> for SensorError {
    fn from(error: I2cError) -> Self {
        Self::I2c(error)
    }
}

/// Peripherals used by the sensor
pub struct SensorPeripherals {
    /// I²C SDA pin
    pub sda: GpioPin<10>,
    /// I²C SCL pin
    pub scl: GpioPin<11>,

    // The pin that enables or disables the pressure sensor
    pub pressure_sensor_enable: GpioPin<18>,

    /// I²C interface
    pub i2c0: I2C0,

    /// Random number generator
    pub rng: Rng,
}

async fn calculate_ads1115_voltage(measured_value: i16) -> f32 {
    // Convert to voltage (ADS1115 is 16-bit, ±2.048V full scale)
    (measured_value as f32 * 2.048) / 32768.0
}

fn calculate_input_voltage_for_voltage_divider(
    output_voltage: f32,
    resistor_before_probe: f32,
    resistor_after_probe: f32,
) -> f32 {
    output_voltage * (resistor_before_probe + resistor_after_probe) / resistor_after_probe
}

fn calculate_water_height_from_pressure_sensor_voltage(
    voltage: f32,
    resistor: f32,
    sensor_maximum_height: f32,
) -> f32 {
    // Constants for 4-20mA sensor
    const MIN_CURRENT: f32 = 0.004; // 4mA
    const MAX_CURRENT: f32 = 0.020; // 20mA
    const CURRENT_RANGE: f32 = MAX_CURRENT - MIN_CURRENT;

    // Calculate minimum voltage (at 4mA)
    let min_voltage = MIN_CURRENT * resistor;

    // Calculate maximum voltage (at 20mA)
    let max_voltage = MAX_CURRENT * resistor;
    let voltage_range = max_voltage - min_voltage;

    // Calculate height
    (voltage - min_voltage) * sensor_maximum_height / voltage_range
}

async fn initialize_bme280(
    bme280: &mut AsyncBme280<I2c<'static, Async>, Delay>,
) -> Result<(), I2cError> {
    info!("Initializing the BME280");
    bme280.init().await?;

    info!("Configuring the BME280");
    bme280
        .set_sampling_configuration(
            Configuration::default()
                .with_temperature_oversampling(Oversampling::Oversample1)
                .with_pressure_oversampling(Oversampling::Oversample1)
                .with_humidity_oversampling(Oversampling::Oversample1)
                .with_sensor_mode(SensorMode::Normal),
        )
        .await?;
    Ok(())
}

async fn read_ads1115(adc: &mut Adc<'_>) -> Result<Ads1115Data, SensorError> {
    info!("Initialize ADS1115 analog-digital converter ...");

    // Generally we try to get 10 measurments per second, so having the converter run at 16 measurements per second is enough
    match adc.set_data_rate(ads1x1x::DataRate16Bit::Sps16) {
        Ok(_) => {
            // Everything is fine. Moving on
            debug!("Set ADS1115 data rate to 16/s.");
        }
        Err(_) => {
            warn!("Failed to set ADS1115 data rate to 16/s. Remains at default.");
        }
    };

    // Our lowest signal is 3V so we drop the ADC back to 2V and use voltage dividers
    match adc.set_full_scale_range(ads1x1x::FullScaleRange::Within2_048V) {
        Ok(_) => {
            // Everything is fine. Moving on
            debug!("Set ADS1115 scale range to 2V.");
        }
        Err(_) => {
            warn!("Failed to set ADS1115 scale range to 2V.");
            return Err(SensorError::FailedToSetAdcRange);
        }
    };

    // Loop around measuring A2 until it stabilizes
    info!("Wait for voltage on ADS1115 A2 to stabilize ...");
    let stabilization_result = wait_for_pressure_sensor_voltage_to_stabilize(adc).await;
    match stabilization_result {
        Ok(_) => info!("Pressure sensor voltage is stable."),
        Err(_) => {
            error!("Pressure sensor voltage is unstable.");
            return Err(SensorError::PressureSensorVoltageNotStable);
        }
    }

    // Then collect data
    info!("Collecting samples from the ADS1115 ...");
    let mut collected_data = Vec::<Ads1115Data, NUMBER_OF_SAMPLES>::new();
    for _n in 0..NUMBER_OF_SAMPLES {
        let sample_result = sample_voltage_data(adc).await;
        match sample_result {
            Ok(r) => drop(collected_data.push(r)),
            Err(error) => error!("Could not sample sensor: {error:?}"),
        }

        let wait_interval = hifitime::Duration::from_seconds(TIME_BETWEEN_SAMPLES_IN_SECONDS);
        info!("Wait {}s for next sample", wait_interval.to_seconds());
        Timer::after(embassy_time::Duration::from_secs(
            wait_interval.to_seconds() as u64,
        ))
        .await;
    }

    // Average the readings. Ideally throw out outliers
    let mut sum_of_brightness: f32 = 0.0;
    let mut sum_of_battery_voltage: f32 = 0.0;
    let mut sum_of_sensor_voltage: f32 = 0.0;
    let mut sum_of_height: f32 = 0.0;
    for n in 0..collected_data.len() {
        let data = &collected_data[n];
        sum_of_brightness += data.enclosure_relative_brightness.get::<percent>();
        sum_of_battery_voltage += data.battery_voltage.get::<volt>();
        sum_of_sensor_voltage += data.pressure_sensor_voltage.get::<volt>();
        sum_of_height += data.height_above_sensor.get::<meter>();
    }

    let number_of_measurements = collected_data.len() as f32;
    let final_brightness = Ratio::new::<percent>(sum_of_brightness / number_of_measurements);
    let final_battery_voltage =
        Voltage::new::<volt>(sum_of_battery_voltage / number_of_measurements);
    let final_sensor_voltage = Voltage::new::<volt>(sum_of_sensor_voltage / number_of_measurements);
    let final_height = Length::new::<meter>(sum_of_height / number_of_measurements);
    let final_data = Ads1115Data::from((
        final_brightness,
        final_battery_voltage,
        final_sensor_voltage,
        final_height,
    ));

    Ok(final_data)
}

async fn read_bme280(
    sensor: &mut AsyncBme280<I2c<'static, Async>, Delay>,
    rng: &mut Rng,
) -> Result<Bme280Data, SensorError> {
    info!("Initialize BME280 environmental sensor ...");

    if let Err(error) = initialize_bme280(sensor).await {
        warn!("Could not initialize BME280 sensor: {error:?}");
    }

    info!(
        "Waiting {}ms for configuration to be processed",
        WARMUP_INTERVAL_IN_MILLISECONDS
    );
    Timer::after(embassy_time::Duration::from_millis(
        WARMUP_INTERVAL_IN_MILLISECONDS as u64,
    ))
    .await;

    let mut collected_data = Vec::<Bme280Data, NUMBER_OF_SAMPLES>::new();
    for _n in 0..NUMBER_OF_SAMPLES {
        let sample_result = sample_environmental_data(sensor, rng).await;
        match sample_result {
            Ok(r) => drop(collected_data.push(r)),
            Err(error) => error!("Could not sample sensor: {error:?}"),
        }

        let wait_interval = hifitime::Duration::from_seconds(TIME_BETWEEN_SAMPLES_IN_SECONDS);
        info!("Wait {}s for next sample", wait_interval.to_seconds());
        Timer::after(embassy_time::Duration::from_secs(
            wait_interval.to_seconds() as u64,
        ))
        .await;
    }

    // Average the readings. Ideally throw out outliers
    let mut sum_of_temperature: f32 = 0.0;
    let mut sum_of_pressure: f32 = 0.0;
    let mut sum_of_humidity: f32 = 0.0;
    for n in 0..collected_data.len() {
        let data = &collected_data[n];
        sum_of_temperature += data.temperature.get::<degree_celsius>();
        sum_of_pressure += data.pressure.get::<hectopascal>();
        sum_of_humidity += data.humidity.get::<percent>();
    }

    let number_of_measurements = collected_data.len() as f32;
    let final_temperature =
        Temperature::new::<degree_celsius>(sum_of_temperature / number_of_measurements);
    let final_pressure = Pressure::new::<hectopascal>(sum_of_pressure / number_of_measurements);
    let final_humidity = Ratio::new::<percent>(sum_of_humidity / number_of_measurements);
    let final_data = Bme280Data::from((final_temperature, final_humidity, final_pressure));

    Ok(final_data)
}

#[embassy_executor::task]
pub async fn read_sensor_data_task(
    peripherals: SensorPeripherals,
    sender: Sender<'static, NoopRawMutex, (Bme280Data, Ads1115Data), 3>,
) {
    info!("Create I²C bus for the BME280");
    let i2c_config = I2cConfig {
        frequency: 25_u32.kHz(),
        ..Default::default()
    };
    let mut i2c = I2c::new(peripherals.i2c0, i2c_config)
        .with_sda(peripherals.sda)
        .with_scl(peripherals.scl)
        .into_async();

    // Read from the BME280
    let mut rng = peripherals.rng;
    let mut bme280_sensor = AsyncBme280::new(i2c, Delay);
    let bme280_data = read_bme280(&mut bme280_sensor, &mut rng).await.unwrap();
    i2c = bme280_sensor.release();

    // power up the pressure sensor
    let mut driver = Output::new(peripherals.pressure_sensor_enable, Level::High);

    // Read from the ADS1115
    let mut ads1115_sensor = Ads1x1x::new_ads1115(i2c, TargetAddr::default());
    let ads1115_data = read_ads1115(&mut ads1115_sensor).await.unwrap();

    // shut down the pressure sensor
    driver.set_low();

    let _ = ads1115_sensor.destroy_ads1115();

    sender.send((bme280_data, ads1115_data)).await;
}

async fn sample_voltage_data(adc: &mut Adc<'_>) -> Result<Ads1115Data, SensorError> {
    info!("Reading voltages from ADS1115 ...");

    // Status of the LDR
    let ldr_voltage = calculate_ads1115_voltage(block!(adc.read(channel::SingleA0)).unwrap()).await;
    let relative_brightness = ldr_voltage / MPU_OUTPUT_VOLTAGE;

    // Status of the battery
    let channel_a3_voltage =
        calculate_ads1115_voltage(block!(adc.read(channel::SingleA3)).unwrap()).await;
    let battery_voltage = calculate_input_voltage_for_voltage_divider(
        channel_a3_voltage,
        VOLTAGE_DIVIDER_BATTERY_RESISTOR_BEFORE_PROBE,
        VOLTAGE_DIVIDER_BATTERY_RESISTOR_AFTER_PROBE,
    );

    // Status of the pressure sensor voltage
    let channel_a2_voltage =
        calculate_ads1115_voltage(block!(adc.read(channel::SingleA2)).unwrap()).await;
    let pressure_sensor_voltage = calculate_input_voltage_for_voltage_divider(
        channel_a2_voltage,
        VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_BEFORE_PROBE,
        VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_AFTER_PROBE,
    );

    // Pressure sensor output
    let channel_a1_voltage =
        calculate_ads1115_voltage(block!(adc.read(channel::SingleA1)).unwrap()).await;
    let pressure_height = calculate_water_height_from_pressure_sensor_voltage(
        channel_a1_voltage,
        PRESSURE_SENSOR_OUTPUT_RESISTOR_AFTER_PROBE,
        PRESSURE_SENSOR_MAXIMUM_HEIGHT,
    );

    let sample = Ads1115Data {
        enclosure_relative_brightness: Ratio::new::<percent>(relative_brightness),
        battery_voltage: Voltage::new::<volt>(battery_voltage),
        pressure_sensor_voltage: Voltage::new::<volt>(pressure_sensor_voltage),
        height_above_sensor: Length::new::<meter>(pressure_height),
    };

    debug!(
        " ┣ Enclosure brightness:    {:.2} V",
        sample.enclosure_relative_brightness.get::<percent>()
    );

    debug!(
        " ┣ Battery voltage:         {:.2} V",
        sample.battery_voltage.get::<volt>()
    );
    debug!(
        " ┣ Pressure sensor voltage: {:.2} V",
        sample.pressure_sensor_voltage.get::<volt>()
    );
    debug!(
        " ┗ Liquid height:           {:.2} m",
        sample.height_above_sensor.get::<meter>()
    );

    Ok(sample)
}

/// Sample sensor and send reading to receiver
async fn sample_environmental_data(
    sensor: &mut AsyncBme280<I2c<'static, Async>, Delay>,
    rng: &mut Rng,
) -> Result<Bme280Data, SensorError> {
    info!("Reading sample ...");

    let sample_result = sensor
        .read_sample()
        .await
        .map_err(SensorError::I2c)
        .and_then(|sample: Bme280Sample| Ok(Bme280Data::try_from(sample)?));
    let sample = sample_result.unwrap_or_else(|error| {
        error!("Cannot read sample: {error:?}");
        warn!("Use a random sample");

        Bme280Data::random(rng)
    });

    debug!(
        " ┣ Temperature: {:.2} C",
        sample.temperature.get::<degree_celsius>()
    );
    debug!(" ┣ Humidity:    {:.2} %", sample.humidity.get::<percent>());
    debug!(
        " ┗ Pressure:    {:.2} hPa",
        sample.pressure.get::<hectopascal>()
    );

    Ok(sample)
}

async fn wait_for_pressure_sensor_voltage_to_stabilize(
    adc: &mut Adc<'_>,
) -> Result<(), SensorError> {
    let mut stable_count = 0;
    loop {
        debug!("Measuring the pressure sensor voltage ...");

        // Status of the pressure sensor voltage
        let channel_a2_voltage =
            calculate_ads1115_voltage(block!(adc.read(channel::SingleA2)).unwrap()).await;
        let pressure_sensor_voltage = calculate_input_voltage_for_voltage_divider(
            channel_a2_voltage,
            VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_BEFORE_PROBE,
            VOLTAGE_DIVIDER_PRESSURE_SENSOR_RESISTOR_AFTER_PROBE,
        );

        debug!("Pressure sensor voltage: {:.2} V", pressure_sensor_voltage);

        let diff = fabsf(EXPECTED_PRESSURE_SENSOR_VOLTAGE - pressure_sensor_voltage);
        if diff < 0.2 {
            stable_count += 1;
        } else {
            stable_count = 0;
        }

        debug!(
            "Pressure sensor voltage has been stable for {} loops",
            stable_count
        );
        if stable_count == 10 {
            break;
        }

        let wait_interval = hifitime::Duration::from_seconds(
            PRESSURE_SENSOR_VOLTAGE_STABILIZATION_CHECK_INTERVAL_IN_SECONDS,
        );
        Timer::after(embassy_time::Duration::from_secs(
            wait_interval.to_seconds() as u64,
        ))
        .await;
    }

    Ok(())
}
