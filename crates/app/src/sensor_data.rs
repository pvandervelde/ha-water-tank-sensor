// Based on code from here: https://github.com/claudiomattera/esp32c3-embassy/

//! Domain types

use esp_hal::rng::Rng;

use hifitime::Epoch;
use uom::si::f32::Pressure;
use uom::si::f32::Ratio as Humidity;
use uom::si::f32::ThermodynamicTemperature as Temperature;
use uom::si::pressure::hectopascal;
use uom::si::ratio::percent;
use uom::si::thermodynamic_temperature::degree_celsius;

use bme280_rs::Sample as Bme280Sample;

/// The number of samples that each measurement should take
pub const NUMBER_OF_SAMPLES: usize = 5;

/// Period to wait between readings (100 milliseconds, aka 0.1 seconds)
pub const TIME_BETWEEN_SAMPLES_IN_SECONDS: f64 = 0.1;

/// The data recorded from the BME280. It provides the environmental data (temperature, pressure, humidity)
/// for the enclosure.
#[derive(Clone, Debug, Default)]
pub struct EnvironmentalData {
    /// Temperature
    pub temperature: Temperature,

    /// Humidity
    pub humidity: Humidity,

    /// Air Pressure
    pub pressure: Pressure,
}

impl EnvironmentalData {
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
            Humidity::new::<percent>(humidity),
            Pressure::new::<hectopascal>(pressure),
        ))
    }
}

impl From<(Temperature, Humidity, Pressure)> for EnvironmentalData {
    fn from((temperature, humidity, pressure): (Temperature, Humidity, Pressure)) -> Self {
        Self {
            temperature,
            humidity,
            pressure,
        }
    }
}

impl TryFrom<Bme280Sample> for EnvironmentalData {
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

/// A reading, i.e. a pair (time, sample)
pub type Reading = (Epoch, EnvironmentalData);

/// An error
#[derive(Debug)]
pub enum Error {
    /// A measurement was missing
    MissingMeasurement,
}
