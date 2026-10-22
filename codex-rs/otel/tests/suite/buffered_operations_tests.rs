//! Exercises global operation buffering across shutdown and rejected configuration changes.

use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::install_global_metrics;
use codex_otel::record_global_operation;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn install() -> MetricsClient {
    install_global_metrics(
        MetricsClient::new(
            MetricsConfig::in_memory("test", "test", "1", Default::default()).with_runtime_reader(),
        )
        .expect("build in-memory metrics"),
    )
}

fn observe() {
    record_global_operation(
        "test.operations",
        "test.duration",
        Duration::from_millis(/*millis*/ 7),
        &[],
    )
    .expect("record an operation");
}

fn totals(metrics: &MetricsClient) -> (u64, u64, f64) {
    let snapshot = metrics.snapshot().expect("collect runtime metrics");
    let mut totals = (0, 0, 0.0);
    for metric in snapshot
        .scope_metrics()
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
    {
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                totals.0 += sum
                    .data_points()
                    .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                    .sum::<u64>();
            }
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                for point in histogram.data_points() {
                    totals.1 += point.count();
                    totals.2 += point.sum();
                }
            }
            _ => panic!("unexpected metric"),
        }
    }
    totals
}

#[test]
fn shutdown_buffers_until_replacement_and_preserves_newer_installations() {
    let first = install();
    first.shutdown().unwrap();
    observe();
    let second = install();
    assert_eq!(totals(&second), (1, 1, 7.0));

    let third = install();
    second.shutdown().unwrap();
    observe();
    assert_eq!(totals(&third), (1, 1, 7.0));
    third.shutdown().unwrap();
}
