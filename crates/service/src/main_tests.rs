use super::*;
use axum::body::Body;
use axum::http::Request;
use hyper::body;
use tower::ServiceExt;

fn create_test_app() -> Router {
    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, hyper::Body>(https);

    let state = AppState {
        metrics: Arc::new(MetricsState::new()),
        observability: Arc::new(ObservabilityConfig {
            metrics_push_url: "http://localhost:9009/api/v1/push".to_string(),
            api_token: "test".to_string(),
            trace_push_url: "http://localhost:4317".to_string(),
            logs_push_url: "http://localhost:3100".to_string(),
        }),
        http_client: client,
    };

    Router::new()
        .route("/api/v1/sensor", post(handle_sensor_data))
        .route("/health", get(health_check))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

#[tokio::test]
async fn test_health_check() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body::to_bytes(response.into_body()).await.unwrap();
    let response: ApiResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.status, "success");
}

#[tokio::test]
async fn test_valid_sensor_data() {
    let app = create_test_app();

    let sensor_data = SensorData {
        device_id: "test_device".to_string(),
        temperature_in_celcius: 25.0,
        humidity_in_percent: 50.0,
        pressure_in_pascal: 1013.0,
    };

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sensor")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&sensor_data).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_invalid_sensor_data() {
    let app = create_test_app();

    let sensor_data = SensorData {
        device_id: "test_device".to_string(),
        temperature_in_celcius: -100.0, // Invalid temperature
        humidity_in_percent: 50.0,
        pressure_in_pascal: 1013.0,
    };

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sensor")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&sensor_data).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
