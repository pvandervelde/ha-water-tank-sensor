// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Task for reading sensor value

use heapless::Vec;
use hifitime::Duration;
use hifitime::Epoch;
use log::debug;
use log::error;
use log::info;
use log::warn;

use embassy_time::Delay;
use embassy_time::Timer;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Sender;

use esp_hal::i2c::master::Error as I2cError;
use esp_hal::i2c::master::I2c;
use esp_hal::rng::Rng;
use esp_hal::Async;

use bme280_rs::AsyncBme280;
use bme280_rs::Configuration;
use bme280_rs::Oversampling;
use bme280_rs::Sample as Bme280Sample;
use bme280_rs::SensorMode;

use uom::si::f32::Pressure;
use uom::si::f32::Ratio as Humidity;
use uom::si::f32::ThermodynamicTemperature as Temperature;
use uom::si::pressure::hectopascal;
use uom::si::pressure::pascal;
use uom::si::ratio::percent;
use uom::si::thermodynamic_temperature::degree_celsius;
use uom::si::thermodynamic_temperature::kelvin;

use crate::clock::Clock;
use crate::clock::Error as ClockError;
use crate::sensor_data::EnvironmentalData;
use crate::sensor_data::Error as DomainError;
use crate::sensor_data::Reading;
use crate::sensor_data::NUMBER_OF_SAMPLES;
use crate::sensor_data::TIME_BETWEEN_SAMPLES_IN_SECONDS;

/// Interval to wait for sensor warmup, 10 milliseconds (aka 0.01 seconds)
const WARMUP_INTERVAL_IN_MILLISECONDS: f64 = 10.0;

/// Error within sensor sampling
#[derive(Debug)]
enum SensorError {
    /// Error from clock
    Clock(#[expect(unused, reason = "Never read directly")] ClockError),

    /// Error from domain
    Domain(DomainError),

    /// Error from I²C bus
    I2c(#[expect(unused, reason = "Never read directly")] I2cError),
}

impl From<ClockError> for SensorError {
    fn from(error: ClockError) -> Self {
        Self::Clock(error)
    }
}

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

/// Task for sampling sensor
#[embassy_executor::task]
pub async fn read_environmental_data_task(
    i2c: I2c<'static, Async>,
    mut rng: Rng,
    sender: Sender<'static, NoopRawMutex, Reading, 3>,
    clock: Clock,
) {
    info!("Create");
    let mut sensor = AsyncBme280::new(i2c, Delay);

    if let Err(error) = initialize_bme280(&mut sensor).await {
        warn!("Could not initialize sensor: {error:?}");
    }

    info!(
        "Waiting {}ms for configuration to be processed",
        WARMUP_INTERVAL_IN_MILLISECONDS
    );
    Timer::after(embassy_time::Duration::from_millis(
        WARMUP_INTERVAL_IN_MILLISECONDS as u64,
    ))
    .await;

    let mut collected_data = Vec::<Reading, NUMBER_OF_SAMPLES>::new();
    for n in 0..NUMBER_OF_SAMPLES {
        let sample_result = sample_environmental_data(&mut sensor, &mut rng, &clock).await;
        match sample_result {
            Ok(r) => drop(collected_data.push(r)),
            Err(error) => error!("Could not sample sensor: {error:?}"),
        }

        let wait_interval = clock.duration_to_next_rounded_wakeup(
            hifitime::Duration::from_seconds(TIME_BETWEEN_SAMPLES_IN_SECONDS),
        );
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
        sum_of_temperature += data.1.temperature.get::<degree_celsius>();
        sum_of_pressure += data.1.pressure.get::<hectopascal>();
        sum_of_humidity += data.1.humidity.get::<percent>();
    }
    let duration = collected_data.last().unwrap().0 - collected_data.first().unwrap().0;
    let half_duration = Duration::from_seconds(duration.to_seconds() / 2.0);
    let recording_time = collected_data.first().unwrap().0 + half_duration;

    let number_of_measurements = collected_data.len() as f32;
    let final_temperature =
        Temperature::new::<degree_celsius>(sum_of_temperature / number_of_measurements);
    let final_pressure = Pressure::new::<hectopascal>(sum_of_pressure / number_of_measurements);
    let final_humidity = Humidity::new::<percent>(sum_of_humidity / number_of_measurements);
    let final_data = EnvironmentalData::from((final_temperature, final_humidity, final_pressure));

    sender.send((recording_time, final_data)).await;
}

/// Sample sensor and send reading to receiver
async fn sample_environmental_data(
    sensor: &mut AsyncBme280<I2c<'static, Async>, Delay>,
    rng: &mut Rng,
    clock: &Clock,
) -> Result<Reading, SensorError> {
    info!("Reading sample ...");

    let now = clock.now();

    let sample_result = sensor
        .read_sample()
        .await
        .map_err(SensorError::I2c)
        .and_then(|sample: Bme280Sample| Ok(EnvironmentalData::try_from(sample)?));
    let sample = sample_result.unwrap_or_else(|error| {
        error!("Cannot read sample: {error:?}");
        warn!("Use a random sample");

        EnvironmentalData::random(rng)
    });

    debug!("Received sample at {:?}", now);
    debug!(
        " ┣ Temperature: {:.2} C",
        sample.temperature.get::<degree_celsius>()
    );
    debug!(" ┣ Humidity:    {:.2} %", sample.humidity.get::<percent>());
    debug!(
        " ┗ Pressure:    {:.2} hPa",
        sample.pressure.get::<hectopascal>()
    );

    Ok((now, sample))
}

/// Initialize sensor
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

//ADS1115
