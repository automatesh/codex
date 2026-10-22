use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::Result;
use codex_otel::metrics_data::AggregatedMetrics;
use codex_otel::metrics_data::InMemoryMetricExporter;
use codex_otel::metrics_data::KeyValue;
use codex_otel::metrics_data::Metric;
use codex_otel::metrics_data::MetricData;
use codex_otel::metrics_data::ResourceMetrics;
use std::collections::BTreeMap;

pub(crate) fn build_metrics_with_defaults(
    default_tags: &[(&str, &str)],
) -> Result<(MetricsClient, InMemoryMetricExporter)> {
    let exporter = InMemoryMetricExporter::default();
    let mut config = MetricsConfig::in_memory(
        "test",
        "codex-cli",
        env!("CARGO_PKG_VERSION"),
        exporter.clone(),
    );
    for (key, value) in default_tags {
        config = config.with_tag(*key, *value)?;
    }
    let metrics = MetricsClient::new(config)?;
    Ok((metrics, exporter))
}

pub(crate) fn latest_metrics(exporter: &InMemoryMetricExporter) -> ResourceMetrics {
    exporter
        .get_finished_metrics()
        .expect("finished metrics should be available")
        .into_iter()
        .last()
        .expect("metrics export should exist")
}

pub(crate) fn find_metric<'a>(
    resource_metrics: &'a ResourceMetrics,
    name: &str,
) -> Option<&'a Metric> {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return Some(metric);
            }
        }
    }
    None
}

pub(crate) fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

pub(crate) fn histogram_data(
    resource_metrics: &ResourceMetrics,
    name: &str,
) -> (Vec<f64>, Vec<u64>, f64, u64) {
    let metric = find_metric(resource_metrics, name).expect("metric should exist");
    match metric.data() {
        AggregatedMetrics::F64(data) => match data {
            MetricData::Histogram(histogram) => {
                let points: Vec<_> = histogram.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                let bounds = point.bounds().collect();
                let bucket_counts = point.bucket_counts().collect();
                (bounds, bucket_counts, point.sum(), point.count())
            }
            _ => panic!("unexpected histogram aggregation"),
        },
        _ => panic!("unexpected metric data type"),
    }
}

pub(crate) fn capture_events(operation: impl FnOnce()) -> Vec<String> {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;
    struct Events(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Events(Arc::clone(&events)));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "test_collector", "collector is active");
        operation();
    });
    let mut events = events.lock().unwrap().clone();
    assert_eq!(events.remove(0), "test_collector");
    events
}
