use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::IntoMakeService;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use std::str::FromStr;
use tower::service_fn;
use tower::ServiceExt;

// SensorData

fn create_valid_sensor_data() -> SensorData {
    SensorData {
        device_id: "test-device-001".to_string(),
        firmware_version: "1.0.0".to_string(),
        boot_count: 1,
        unix_time_in_seconds: 1735642800.0 + 3600.0, // Jan 1st 2025 + 1 hour
        temperature_in_celcius: 25.0,
        humidity_in_percent: 50.0,
        pressure_in_pascal: 101325.0, // standard atmospheric pressure
        battery_voltage: 3.7,
        pressure_sensor_voltage: 5.0,
        tank_level_in_meters: 1.5,
        tank_temperature_in_celcius: 20.0,
    }
}

#[test]
fn test_valid_sensor_data() {
    let data = create_valid_sensor_data();
    assert!(
        data.validate().is_ok(),
        "Valid sensor data should validate successfully"
    );
}

#[test]
fn test_invalid_boot_count() {
    let mut data = create_valid_sensor_data();
    data.boot_count = 0;
    let result = data.validate();
    assert!(result.is_err(), "Boot count of 0 should be invalid");
    assert_eq!(
        result.unwrap_err(),
        "The device boot count should at least be 1.".to_string()
    );
}

#[test]
fn test_invalid_timestamp() {
    let mut data = create_valid_sensor_data();
    data.unix_time_in_seconds = 1735642799.0; // Just before Jan 1st 2025
    let result = data.validate();
    assert!(
        result.is_err(),
        "Timestamp before Jan 1st 2025 should be invalid"
    );
    assert_eq!(result.unwrap_err(), "Invalid timestamp".to_string());
}

#[test]
fn test_invalid_temperature() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.temperature_in_celcius = -51.0;
    assert!(
        data.validate().is_err(),
        "Temperature below -50°C should be invalid"
    );

    // Test too high
    data.temperature_in_celcius = 100.1;
    assert!(
        data.validate().is_err(),
        "Temperature above 100°C should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Temperature out of reasonable range (-50°C to 100°C)".to_string()
    );
}

#[test]
fn test_invalid_humidity() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.humidity_in_percent = -0.1;
    assert!(
        data.validate().is_err(),
        "Humidity below 0% should be invalid"
    );

    // Test too high
    data.humidity_in_percent = 100.1;
    assert!(
        data.validate().is_err(),
        "Humidity above 100% should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Humidity must be between 0% and 100%".to_string()
    );
}

#[test]
fn test_invalid_pressure() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.pressure_in_pascal = 49.9e3;
    assert!(
        data.validate().is_err(),
        "Pressure below 50kPa should be invalid"
    );

    // Test too high
    data.pressure_in_pascal = 150.1e3;
    assert!(
        data.validate().is_err(),
        "Pressure above 150kPa should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Pressure out of reasonable range (800-1200 hPa)".to_string()
    );
}

#[test]
fn test_invalid_battery_voltage() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.battery_voltage = -0.1;
    assert!(
        data.validate().is_err(),
        "Battery voltage below 0V should be invalid"
    );

    // Test too high
    data.battery_voltage = 15.1;
    assert!(
        data.validate().is_err(),
        "Battery voltage above 15V should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Battery voltage out of reasonable range (0.0V to 15.0V)".to_string()
    );
}

#[test]
fn test_invalid_pressure_sensor_voltage() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.pressure_sensor_voltage = -0.1;
    assert!(
        data.validate().is_err(),
        "Pressure sensor voltage below 0V should be invalid"
    );

    // Test too high
    data.pressure_sensor_voltage = 32.1;
    assert!(
        data.validate().is_err(),
        "Pressure sensor voltage above 32V should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Pressure sensor voltage out of reasonable range (0.0V to 32.0V)".to_string()
    );
}

#[test]
fn test_invalid_tank_level() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.tank_level_in_meters = -0.1;
    assert!(
        data.validate().is_err(),
        "Tank level below 0m should be invalid"
    );

    // Test too high
    data.tank_level_in_meters = 5.1;
    assert!(
        data.validate().is_err(),
        "Tank level above 5m should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Tank water level out of reasonable range (0.0m to 5.0m)".to_string()
    );
}

#[test]
fn test_invalid_tank_temperature() {
    // Test too low
    let mut data = create_valid_sensor_data();
    data.tank_temperature_in_celcius = -50.1;
    assert!(
        data.validate().is_err(),
        "Tank temperature below -50°C should be invalid"
    );

    // Test too high
    data.tank_temperature_in_celcius = 100.1;
    assert!(
        data.validate().is_err(),
        "Tank temperature above 100°C should be invalid"
    );

    // Test error message
    let result = data.validate();
    assert_eq!(
        result.unwrap_err(),
        "Tank water temperature out of reasonable range (-50°C to 100°C)".to_string()
    );
}

#[test]
fn test_boundary_values() {
    let mut data = create_valid_sensor_data();

    // Test lower boundaries
    data.boot_count = 1;
    data.unix_time_in_seconds = 1735642800.0;
    data.temperature_in_celcius = -50.0;
    data.humidity_in_percent = 0.0;
    data.pressure_in_pascal = 50.0e3;
    data.battery_voltage = 0.0;
    data.pressure_sensor_voltage = 0.0;
    data.tank_level_in_meters = 0.0;
    data.tank_temperature_in_celcius = -50.0;
    assert!(
        data.validate().is_ok(),
        "Lower boundary values should be valid"
    );

    // Test upper boundaries
    data.temperature_in_celcius = 100.0;
    data.humidity_in_percent = 100.0;
    data.pressure_in_pascal = 150.0e3;
    data.battery_voltage = 15.0;
    data.pressure_sensor_voltage = 32.0;
    data.tank_level_in_meters = 5.0;
    data.tank_temperature_in_celcius = 100.0;
    assert!(
        data.validate().is_ok(),
        "Upper boundary values should be valid"
    );
}

#[test]
fn test_api_response_success() {
    let response = ApiResponse::success("Test message");
    assert_eq!(response.status, "success");
    assert_eq!(response.message, "Test message");
    // We can't easily test the exact timestamp, but we can check it's not empty
    assert!(!response.timestamp.is_empty());
}

#[test]
fn test_api_response_error() {
    let response = ApiResponse::error("Error message");
    assert_eq!(response.status, "error");
    assert_eq!(response.message, "Error message");
    assert!(!response.timestamp.is_empty());
}

#[tokio::test]
async fn test_health_check() {
    // Initialize tracing for the test
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let response = health_check().await.into_response();
    assert_eq!(response.status(), StatusCode::OK);

    // Convert the response body to bytes and then to a string
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();

    // Parse the JSON response
    let api_response: ApiResponse = serde_json::from_str(body_str.as_str()).unwrap();
    assert_eq!(api_response.status, "success");
    assert_eq!(api_response.message, "Service is healthy");
}

#[tokio::test]
async fn test_handle_sensor_data_valid() {
    // Initialize tracing for the test
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    // Initialize global meter provider for the test
    let meter_provider = SdkMeterProvider::builder().build();
    global::set_meter_provider(meter_provider);

    let valid_data = SensorData {
        device_id: "test-device-001".to_string(),
        firmware_version: "1.0.0".to_string(),
        boot_count: 1,
        unix_time_in_seconds: 1735642800.0 + 3600.0, // Jan 1st 2025 + 1 hour
        temperature_in_celcius: 25.0,
        humidity_in_percent: 50.0,
        pressure_in_pascal: 101325.0, // standard atmospheric pressure
        battery_voltage: 3.7,
        pressure_sensor_voltage: 5.0,
        tank_level_in_meters: 1.5,
        tank_temperature_in_celcius: 20.0,
    };

    let result = handle_sensor_data(Json(valid_data)).await;
    assert!(
        result.is_ok(),
        "Valid sensor data should be processed successfully"
    );

    let status = result.unwrap().into_response();
    assert_eq!(status.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_handle_sensor_data_invalid() {
    // Initialize tracing for the test
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let invalid_data = SensorData {
        device_id: "test-device-001".to_string(),
        firmware_version: "1.0.0".to_string(),
        boot_count: 0, // Invalid boot count
        unix_time_in_seconds: 1735642800.0 + 3600.0,
        temperature_in_celcius: 25.0,
        humidity_in_percent: 50.0,
        pressure_in_pascal: 101325.0,
        battery_voltage: 3.7,
        pressure_sensor_voltage: 5.0,
        tank_level_in_meters: 1.5,
        tank_temperature_in_celcius: 20.0,
    };

    let result = handle_sensor_data(Json(invalid_data)).await;

    match result {
        Ok(_) => assert!(false, "Invalid sensor data should be rejected"),
        Err((status, _)) => assert_eq!(status, StatusCode::BAD_REQUEST),
    }
}

#[test]
fn test_record_gauge() {
    // Initialize a meter provider
    let provider = SdkMeterProvider::builder().build();
    let meter = provider.meter("test");

    // Test recording a gauge
    record_gauge(
        &meter,
        "test_gauge".to_string(),
        "Test description".to_string(),
        Some("unit".to_string()),
        42.0,
    );

    // We can't easily assert the recorded value in tests,
    // but we can verify the code runs without errors
}

#[test]
fn test_observability_config_from_env() {
    // Save original environment
    let original_metrics = std::env::var("METRICS_PUSH_URL").ok();
    let original_tracing = std::env::var("TRACING_PUSH_URL").ok();
    let original_logs = std::env::var("LOGS_PUSH_URL").ok();

    // Set test environment variables
    std::env::set_var("METRICS_PUSH_URL", "http://test-metrics:4317");
    std::env::set_var("TRACING_PUSH_URL", "http://test-tracing:4317");
    std::env::set_var("LOGS_PUSH_URL", "http://test-logs:4317");

    let config = ObservabilityConfig {
        metrics_push_url: std::env::var("METRICS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        trace_push_url: std::env::var("TRACING_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        logs_push_url: std::env::var("LOGS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
    };

    assert_eq!(config.metrics_push_url, "http://test-metrics:4317");
    assert_eq!(config.trace_push_url, "http://test-tracing:4317");
    assert_eq!(config.logs_push_url, "http://test-logs:4317");

    // Restore original environment
    match original_metrics {
        Some(val) => std::env::set_var("METRICS_PUSH_URL", val),
        None => std::env::remove_var("METRICS_PUSH_URL"),
    }
    match original_tracing {
        Some(val) => std::env::set_var("TRACING_PUSH_URL", val),
        None => std::env::remove_var("TRACING_PUSH_URL"),
    }
    match original_logs {
        Some(val) => std::env::set_var("LOGS_PUSH_URL", val),
        None => std::env::remove_var("LOGS_PUSH_URL"),
    }
}

#[test]
fn test_observability_config_defaults() {
    // Save original environment
    let original_metrics = std::env::var("METRICS_PUSH_URL").ok();
    let original_tracing = std::env::var("TRACING_PUSH_URL").ok();
    let original_logs = std::env::var("LOGS_PUSH_URL").ok();

    // Remove environment variables to test defaults
    std::env::remove_var("METRICS_PUSH_URL");
    std::env::remove_var("TRACING_PUSH_URL");
    std::env::remove_var("LOGS_PUSH_URL");

    let config = ObservabilityConfig {
        metrics_push_url: std::env::var("METRICS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        trace_push_url: std::env::var("TRACING_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
        logs_push_url: std::env::var("LOGS_PUSH_URL")
            .unwrap_or_else(|_| "http://localhost:4317".to_string()),
    };

    assert_eq!(config.metrics_push_url, "http://localhost:4317");
    assert_eq!(config.trace_push_url, "http://localhost:4317");
    assert_eq!(config.logs_push_url, "http://localhost:4317");

    // Restore original environment
    match original_metrics {
        Some(val) => std::env::set_var("METRICS_PUSH_URL", val),
        None => std::env::remove_var("METRICS_PUSH_URL"),
    }
    match original_tracing {
        Some(val) => std::env::set_var("TRACING_PUSH_URL", val),
        None => std::env::remove_var("TRACING_PUSH_URL"),
    }
    match original_logs {
        Some(val) => std::env::set_var("LOGS_PUSH_URL", val),
        None => std::env::remove_var("LOGS_PUSH_URL"),
    }
}
