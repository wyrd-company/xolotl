//! A separate process installs the global meter before any application
//! instruments are initialized, and reads the exported counters after real
//! refreshes and gateway requests (including consumed streaming bodies).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
use axum::http::{HeaderMap, Method};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use xolotl::catalog::ModelCatalog;
use xolotl::config::{CatalogConfig, GatewayConfig, ProviderConfig, ProviderKind};
use xolotl::db::Db;
use xolotl::gateway::Gateway;
use xolotl::policy::Target;
use xolotl::secrets::{MemoryStore, Secrets};

type Counts = BTreeMap<BTreeMap<String, String>, u64>;

fn counts(provider: &SdkMeterProvider, exporter: &InMemoryMetricExporter, name: &str) -> Counts {
    provider.force_flush().unwrap();
    let batches = exporter.get_finished_metrics().unwrap();
    let Some(batch) = batches.last() else { return Counts::new() };
    batch
        .scope_metrics()
        .flat_map(|scope| scope.metrics())
        .filter(|metric| metric.name() == name)
        .flat_map(|metric| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else { panic!("expected a u64 counter") };
            assert!(sum.is_monotonic());
            sum.data_points().map(|point| (point.attributes().map(|a| (a.key.to_string(), a.value.to_string())).collect(), point.value()))
        })
        .collect()
}

fn attributes(attrs: &[(&str, &str)]) -> BTreeMap<String, String> {
    attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn refresh(server: &MockServer, catalog: &ModelCatalog, http: &reqwest::Client, response: ResponseTemplate) {
    server.reset().await;
    Mock::given(method("GET")).and(path("/metadata")).respond_with(response).expect(1).mount(server).await;
    catalog.refresh_metadata(http).await;
    server.verify().await;
}

async fn request(server: &MockServer, gateway: &Gateway, model: &str, usage: Value, stream: bool) {
    server.reset().await;
    let payload = json!({"choices":[], "usage":usage});
    let response = if stream {
        ResponseTemplate::new(200).set_body_raw(format!("data: {payload}\n\ndata: [DONE]\n\n"), "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(payload)
    };
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(response).expect(1).mount(server).await;
    let target = Target { provider: "sample".into(), model: model.into() };
    let response = gateway
        .execute(
            &target,
            Method::POST,
            "/v1/chat/completions",
            HeaderMap::new(),
            json!({"model":model,"messages":[],"stream":stream}),
            Some("test/client"),
            model,
            None,
            None,
        )
        .await;
    assert_eq!(response.status(), 200);
    assert!(!to_bytes(response.into_body(), usize::MAX).await.unwrap().is_empty());
    server.verify().await;
}

#[tokio::test]
async fn pricing_metrics_report_refresh_outcomes_and_request_miss_reasons() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).with_interval(Duration::from_secs(3600)).build())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    let server = MockServer::start().await;
    let http = xolotl::proxy::upstream_client(Duration::from_secs(5)).unwrap();
    let mut cfg = CatalogConfig { metadata_url: String::new(), ..Default::default() };
    ModelCatalog::new(cfg.clone(), BTreeMap::new()).refresh_metadata(&http).await;
    assert!(counts(&provider, &exporter, "xolotl.pricing.refreshes").is_empty(), "disabled pricing is not an attempt");

    // Configuration overrides also count as a metadata match.
    cfg.metadata.insert(
        "sample/override-only".into(),
        xolotl::metadata::Override {
            values: xolotl::metadata::Metadata { input_cost_per_token: Some(0.1), ..Default::default() },
            ..Default::default()
        },
    );
    cfg.metadata_url = format!("{}/metadata", server.uri());
    let catalog = Arc::new(ModelCatalog::new(cfg, BTreeMap::new()));
    // Isolated malformed fields/entries do not make a usable refresh fail.
    let registry = json!({
        "priced":{"input_cost_per_token":0.1,"output_cost_per_token":0.2},
        "input-only":{"input_cost_per_token":0.1,"output_cost_per_token":-1},
        "output-only":{"output_cost_per_token":0.2},
        "context-only":{"max_input_tokens":1000},
        "free":{"input_cost_per_token":0,"output_cost_per_token":0},
        "empty":{}, "malformed":42
    });
    let mut outcomes = Counts::new();
    for (response, result) in [
        (ResponseTemplate::new(200).set_body_json(registry), "success"),
        (ResponseTemplate::new(503), "fetch_error"),
        (ResponseTemplate::new(200).set_body_raw("not JSON", "application/json"), "validation_error"),
        (ResponseTemplate::new(200).set_body_json(json!([])), "validation_error"),
        (ResponseTemplate::new(200).set_body_json(json!({"m":{"input_cost_per_token":-1}})), "validation_error"),
    ] {
        refresh(&server, &catalog, &http, response).await;
        *outcomes.entry(attributes(&[("result", result)])).or_default() += 1;
        assert_eq!(counts(&provider, &exporter, "xolotl.pricing.refreshes"), outcomes);
    }
    assert_eq!(catalog.metadata("sample", "priced").values.input_cost_per_token, Some(0.1), "failed refresh keeps usable prices");

    let bad_url = CatalogConfig { metadata_url: "://invalid".into(), ..Default::default() };
    ModelCatalog::new(bad_url, BTreeMap::new()).refresh_metadata(&http).await;
    *outcomes.entry(attributes(&[("result", "fetch_error")])).or_default() += 1;
    assert_eq!(counts(&provider, &exporter, "xolotl.pricing.refreshes"), outcomes);

    let secrets: Secrets = Arc::new(MemoryStore::default());
    secrets.put("providers/sample", json!({"api_key":"test"}).as_object().unwrap().clone()).await.unwrap();
    let gateway_cfg = GatewayConfig {
        providers: BTreeMap::from([(
            "sample".into(),
            ProviderConfig { kind: ProviderKind::OpenaiCompatible, base_url: Some(format!("{}/v1", server.uri())), ..Default::default() },
        )]),
    };
    let gateway = Gateway::new(&gateway_cfg, secrets, http, Db::in_memory().await.unwrap()).with_catalog(catalog);
    let mut misses = Counts::new();
    // Both buffered and streamed responses use the same accounting. Prices are
    // required only for token categories with a nonzero, clamped token count.
    for stream in [false, true] {
        for (model, usage, reason) in [
            ("unknown", json!({"prompt_tokens":10,"completion_tokens":2}), Some("model_not_found")),
            ("input-only", json!({"prompt_tokens":10,"completion_tokens":2}), Some("missing_rate")),
            ("output-only", json!({"prompt_tokens":10,"completion_tokens":2}), Some("missing_rate")),
            ("context-only", json!({"prompt_tokens":10,"completion_tokens":2}), Some("missing_rate")),
            ("empty", json!({"prompt_tokens":10,"completion_tokens":2}), Some("missing_rate")),
            ("override-only", json!({"prompt_tokens":10,"completion_tokens":2}), Some("missing_rate")),
            ("priced", json!({"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}), Some("missing_rate")),
            ("priced", json!({"input_tokens":10,"output_tokens":2,"cache_creation_input_tokens":4}), Some("missing_rate")),
            ("priced", json!({"prompt_tokens":10,"completion_tokens":2}), None),
            ("free", json!({"prompt_tokens":10,"completion_tokens":2}), None),
            ("input-only", json!({"prompt_tokens":10,"completion_tokens":0}), None),
            ("override-only", json!({"prompt_tokens":10,"completion_tokens":0}), None),
            ("output-only", json!({"prompt_tokens":0,"completion_tokens":2}), None),
            ("unknown", json!({"prompt_tokens":0,"completion_tokens":0}), None),
            ("priced", Value::Null, None),
            ("priced", json!({"prompt_tokens":10}), None),
            ("priced", json!({"completion_tokens":2}), None),
            ("unknown", Value::Null, None),
        ] {
            request(&server, &gateway, model, usage, stream).await;
            if let Some(reason) = reason {
                *misses.entry(attributes(&[("provider", "sample"), ("model", model), ("reason", reason)])).or_default() += 1;
            }
            assert_eq!(counts(&provider, &exporter, "xolotl.pricing.misses"), misses, "model={model}, stream={stream}");
        }
    }
    println!("Recorded refresh outcomes: {outcomes:?}\nRecorded request pricing misses: {misses:?}");
    provider.shutdown().unwrap();
}
