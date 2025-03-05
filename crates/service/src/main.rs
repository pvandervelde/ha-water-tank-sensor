// time
use chrono::Utc;

// REST
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};

use once_cell::sync::Lazy;

// HTTP
use tower_http::trace::TraceLayer;

// JSON
use serde::{Deserialize, Serialize};

// Observability
use opentelemetry::KeyValue;
use opentelemetry::{global, InstrumentationScope};
use opentelemetry::{metrics::Meter, trace::TraceError};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::metrics::{MetricError, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::{
    logs::{LogError, LoggerProvider},
    metrics::Temporality,
};
use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};
use tracing::{debug, error, info, instrument};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{prelude::*, EnvFilter};

// Error handling
use anyhow::Result;

#[cfg(test)]
#[path = "main_tests.rs"]
mod main_tests;

static RESOURCE: Lazy<Resource> = Lazy::new(|| {
    Resource::new(vec![KeyValue::new(
        opentelemetry_semantic_conventions::resource::SERVICE_NAME,
        "tank-sensor-service",
    )])
});

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
struct SensorData {
    device_id: String,
    firmware_version: String,
    boot_count: u32,
    run_time_in_seconds: f64,
    wifi_start_time_in_seconds: f64,
    temperature_in_celcius: f32,
    humidity_in_percent: f32,
    pressure_in_pascal: f32,
    battery_voltage: f32,
    pressure_sensor_voltage: f32,
    tank_level_in_meters: f32,
    tank_temperature_in_celcius: f32,
}

impl SensorData {
    fn validate(&self) -> Result<(), String> {
        if self.boot_count < 1 {
            return Err("The device boot count should at least be 1.".to_string());
        }

        if self.run_time_in_seconds < 0.0 {
            return Err("Run time out of reasonable range (> 0.0)".to_string());
        }

        if self.wifi_start_time_in_seconds < 0.0 {
            return Err("Wifi start time out of reasonable range (> 0.0)".to_string());
        }

        if self.temperature_in_celcius < -50.0 || self.temperature_in_celcius > 100.0 {
            return Err("Temperature out of reasonable range (-50°C to 100°C)".to_string());
        }

        if self.humidity_in_percent < 0.0 || self.humidity_in_percent > 100.0 {
            return Err("Humidity must be between 0% and 100%".to_string());
        }

        if self.pressure_in_pascal < 50.0e3 || self.pressure_in_pascal > 150.0e3 {
            return Err("Pressure out of reasonable range (500-1500 hPa)".to_string());
        }

        if self.battery_voltage < 0.0 || self.battery_voltage > 15.0 {
            return Err("Battery voltage out of reasonable range (0.0V to 15.0V)".to_string());
        }

        if self.pressure_sensor_voltage < 0.0 || self.pressure_sensor_voltage > 32.0 {
            return Err(
                "Pressure sensor voltage out of reasonable range (0.0V to 32.0V)".to_string(),
            );
        }

        if self.tank_level_in_meters < 0.0 || self.tank_level_in_meters > 5.0 {
            return Err("Tank water level out of reasonable range (0.0m to 5.0m)".to_string());
        }

        if self.tank_temperature_in_celcius < -50.0 || self.tank_temperature_in_celcius > 100.0 {
            return Err(
                "Tank water temperature out of reasonable range (-50°C to 100°C)".to_string(),
            );
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiResponse {
    status: String,
    timestamp: String,
    message: String,
}

impl ApiResponse {
    fn success(message: impl Into<String>) -> Self {
        Self {
            status: "success".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
struct ObservabilityConfig {
    metrics_push_url: String,
    trace_push_url: String,
    logs_push_url: String,
}

#[instrument(fields())]
async fn handle_health_check() -> impl IntoResponse {
    info!("Health check request received");
    (
        StatusCode::OK,
        Json(ApiResponse::success("Service is healthy")),
    )
}

#[instrument(fields(device_id = %sensor_data.device_id))]
async fn handle_sensor_data(
    Json(sensor_data): Json<SensorData>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiResponse>)> {
    // Validate sensor data
    if let Err(e) = sensor_data.validate() {
        error!(error = %e, "Invalid sensor data received");
        return Err((StatusCode::BAD_REQUEST, Json(ApiResponse::error(e))));
    }

    let device_scope_attributes = vec![
        KeyValue::new(
            opentelemetry_semantic_conventions::resource::DEVICE_ID,
            sensor_data.device_id.clone(),
        ),
        KeyValue::new(
            opentelemetry_semantic_conventions::resource::DEVICE_MODEL_NAME,
            "ha-tank-sensor",
        ),
    ];
    let scope = InstrumentationScope::builder("tank_level_device")
        .with_version(sensor_data.firmware_version.clone())
        .with_attributes(device_scope_attributes)
        .build();

    let meter = global::meter_with_scope(scope);
    record_sensor_metrics(&meter, &sensor_data);

    Ok((
        StatusCode::OK,
        Json(ApiResponse::success(
            "Data received and processed successfully",
        )),
    ))
}

fn init_logs(
    config: &ObservabilityConfig,
) -> Result<opentelemetry_sdk::logs::LoggerProvider, LogError> {
    debug!("Sending logs to: {}", config.logs_push_url.clone());
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(config.logs_push_url.clone())
        .build()?;

    Ok(LoggerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build())
}

fn init_metrics(
    config: &ObservabilityConfig,
) -> Result<opentelemetry_sdk::metrics::SdkMeterProvider, MetricError> {
    debug!("Sending metrics to: {}", config.metrics_push_url.clone());
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(config.metrics_push_url.clone())
        .with_temporality(Temporality::Delta) // Measurements at different times don't mix
        .build()?;

    let reader = PeriodicReader::builder(exporter, runtime::Tokio).build();

    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(RESOURCE.clone())
        .build())
}

fn init_traces(config: &ObservabilityConfig) -> Result<sdktrace::TracerProvider, TraceError> {
    debug!("Sending traces to: {}", config.trace_push_url.clone());
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(config.trace_push_url.clone())
        .build()?;
    Ok(sdktrace::TracerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build())
}

#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .expect("PORT must be a valid port number");

    let config = ObservabilityConfig {
        metrics_push_url: std::env::var("METRICS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        trace_push_url: std::env::var("TRACING_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        logs_push_url: std::env::var("LOGS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
    };

    // Initialize telemetry
    let (logs, metrics, tracing) = setup_telemetry(&config)?;
    info!("Telemetry initialized");

    // Create router with routes
    let app = Router::new()
        .route("/api/v1/sensor", post(handle_sensor_data))
        .route("/api/v1/logs", post(handle_log_data))
        .route("/health", get(handle_health_check))
        .layer(TraceLayer::new_for_http());

    info!("Server starting on port {}", port);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .unwrap();
    axum::serve(listener, app).await?;

    tracing.shutdown()?;
    metrics.shutdown()?;
    logs.shutdown()?;

    Ok(())
}

fn record_gauge<T: Into<f64>>(
    meter: &Meter,
    name: String,
    description: String,
    unit: Option<String>,
    value: T,
) {
    let builder = meter.f64_gauge(name).with_description(description);
    let builder = match unit {
        Some(u) => builder.with_unit(u),
        None => builder,
    };
    let gauge = builder.build();
    gauge.record(value.into(), &[]);
}

fn record_sensor_metrics(meter: &Meter, sensor_data: &SensorData) {
    // Update boot count
    let boot_count = meter
        .u64_gauge("device_boot_count")
        .with_description("The number of times the device has booted")
        .build();
    boot_count.record(sensor_data.boot_count as u64, &[]);

    // Update the gauges
    record_gauge(
        meter,
        "run_time".to_string(),
        "The amount of time, in seconds, that the device has been running".to_string(),
        Some("sec".to_string()),
        sensor_data.run_time_in_seconds,
    );

    record_gauge(
        meter,
        "wifi_start_time".to_string(),
        "The amount of time, in seconds, that the wifi took to get started".to_string(),
        Some("sec".to_string()),
        sensor_data.wifi_start_time_in_seconds,
    );

    record_gauge(
        meter,
        "enclosure_temperature".to_string(),
        "Temperature of the device enclosure in degrees Celcius".to_string(),
        Some("C".to_string()),
        sensor_data.temperature_in_celcius,
    );

    record_gauge(
        meter,
        "enclosure_air_pressure".to_string(),
        "Air pressure in the device enclosure in Pascal".to_string(),
        Some("Pa".to_string()),
        sensor_data.pressure_in_pascal,
    );

    record_gauge(
        meter,
        "enclosure_humidity".to_string(),
        "Humidity (%) in the device enclosure as a percentage".to_string(),
        None,
        sensor_data.humidity_in_percent,
    );

    record_gauge(
        meter,
        "battery_voltage".to_string(),
        "The voltage of the device battery in Volts.".to_string(),
        Some("V".to_string()),
        sensor_data.battery_voltage,
    );

    record_gauge(
        meter,
        "pressure_sensor_voltage".to_string(),
        "The voltage for the pressure sensor in Volts.".to_string(),
        Some("V".to_string()),
        sensor_data.pressure_sensor_voltage,
    );

    record_gauge(
        meter,
        "water_level".to_string(),
        "The level of the water in the tank".to_string(),
        Some("m".to_string()),
        sensor_data.tank_level_in_meters,
    );

    record_gauge(
        meter,
        "water_temperature".to_string(),
        "The temperature of the water in the tank".to_string(),
        Some("C".to_string()),
        sensor_data.tank_temperature_in_celcius,
    );
}

fn setup_telemetry(
    config: &ObservabilityConfig,
) -> Result<(LoggerProvider, SdkMeterProvider, sdktrace::TracerProvider)> {
    let logger_provider = init_logs(config)?;

    // Create a new OpenTelemetryTracingBridge using the above LoggerProvider.
    let otel_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    // For the OpenTelemetry layer, add a tracing filter to filter events from
    // OpenTelemetry and its dependent crates (opentelemetry-otlp uses crates
    // like reqwest/tonic etc.) from being sent back to OTel itself, thus
    // preventing infinite telemetry generation. The filter levels are set as
    // follows:
    // - Allow `info` level and above by default.
    // - Restrict `opentelemetry`, `hyper`, `tonic`, and `reqwest` completely.
    // Note: This will also drop events from crates like `tonic` etc. even when
    // they are used outside the OTLP Exporter. For more details, see:
    // https://github.com/open-telemetry/opentelemetry-rust/issues/761
    let filter_otel = EnvFilter::new("info")
        .add_directive("hyper=off".parse().unwrap())
        .add_directive("opentelemetry=off".parse().unwrap())
        .add_directive("tonic=off".parse().unwrap())
        .add_directive("h2=off".parse().unwrap())
        .add_directive("reqwest=off".parse().unwrap());
    let otel_layer = otel_layer.with_filter(filter_otel);

    // Create a new tracing::Fmt layer to print the logs to stdout. It has a
    // default filter of `info` level and above, and `debug` and above for logs
    // from OpenTelemetry crates. The filter levels can be customized as needed.
    let filter_fmt = EnvFilter::new("info").add_directive("opentelemetry=debug".parse().unwrap());
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_names(true)
        .with_filter(filter_fmt);

    // Initialize the tracing subscriber with the OpenTelemetry layer and the
    // Fmt layer.
    tracing_subscriber::registry()
        .with(otel_layer)
        .with(fmt_layer)
        .init();

    let tracer_provider = init_traces(config)?;
    global::set_tracer_provider(tracer_provider.clone());

    let meter_provider = init_metrics(config)?;
    global::set_meter_provider(meter_provider.clone());

    Ok((logger_provider, meter_provider, tracer_provider))
}
