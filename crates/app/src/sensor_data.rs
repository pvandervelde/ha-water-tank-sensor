// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Domain types

use esp_hal::rng::Rng;

use uom::si::f32::ElectricPotential as Voltage;
use uom::si::f32::Length;
use uom::si::f32::Pressure;
use uom::si::f32::Ratio;
use uom::si::f32::ThermodynamicTemperature as Temperature;
use uom::si::pressure::hectopascal;
use uom::si::ratio::percent;
use uom::si::thermodynamic_temperature::degree_celsius;

use bme280_rs::Sample as Bme280Sample;

/// The number of samples that each measurement should take
pub const NUMBER_OF_SAMPLES: usize = 5;

/// Period to wait between readings (100 milliseconds, aka 0.1 seconds)
pub const TIME_BETWEEN_SAMPLES_IN_SECONDS: f64 = 0.1;

#[derive(Clone, Debug, Default)]
pub struct Ads1115Data {
    pub enclosure_relative_brightness: Ratio,

    pub battery_voltage: Voltage,

    pub pressure_sensor_voltage: Voltage,

    pub height_above_sensor: Length,
}

impl From<(Ratio, Voltage, Voltage, Length)> for Ads1115Data {
    fn from(
        (
            enclosure_relative_brightness,
            battery_voltage,
            pressure_sensor_voltage,
            height_above_sensor,
        ): (Ratio, Voltage, Voltage, Length),
    ) -> Self {
        Self {
            enclosure_relative_brightness,
            battery_voltage,
            pressure_sensor_voltage,
            height_above_sensor,
        }
    }
}

/// The data recorded from the BME280. It provides the environmental data (temperature, pressure, humidity)
/// for the enclosure.
#[derive(Clone, Debug, Default)]
pub struct Bme280Data {
    /// Temperature
    pub temperature: Temperature,

    /// Humidity
    pub humidity: Ratio,

    /// Air Pressure
    pub pressure: Pressure,
}

impl Bme280Data {
    /// Construct a random sample
    #[expect(clippy::cast_precision_loss, reason = "Acceptable precision loss")]
    pub fn random(rng: &mut Rng) -> Self {
        let temperature_seed = rng.random() as f32 / u32::MAX as f32;
        let humidity_seed = rng.random() as f32 / u32::MAX as f32;
        let pressure_seed = rng.random() as f32 / u32::MAX as f32;

        let temperature = temperature_seed * (30.0 - 15.0) + 15.0;
        let humidity = humidity_seed * (80.0 - 20.0) + 20.0;
        let pressure = pressure_seed * (1010.0 - 990.0) + 990.0;

        Self::from((
            Temperature::new::<degree_celsius>(temperature),
            Ratio::new::<percent>(humidity),
            Pressure::new::<hectopascal>(pressure),
        ))
    }
}

impl From<(Temperature, Ratio, Pressure)> for Bme280Data {
    fn from((temperature, humidity, pressure): (Temperature, Ratio, Pressure)) -> Self {
        Self {
            temperature,
            humidity,
            pressure,
        }
    }
}

impl TryFrom<Bme280Sample> for Bme280Data {
    type Error = Error;

    fn try_from(sample: Bme280Sample) -> Result<Self, Self::Error> {
        let temperature = sample.temperature.ok_or(Self::Error::MissingMeasurement)?;
        let humidity = sample.humidity.ok_or(Self::Error::MissingMeasurement)?;
        let pressure = sample.pressure.ok_or(Self::Error::MissingMeasurement)?;
        Ok(Self {
            temperature,
            humidity,
            pressure,
        })
    }
}

// AD converter data

/// An error
#[derive(Debug)]
pub enum Error {
    /// A measurement was missing
    MissingMeasurement,
}
