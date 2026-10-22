use crate::metrics::config::{MetricsConfig, MetricsExporter};
use crate::metrics::timer::Timer;
use crate::metrics::validation::{
    validate_metric_name, validate_tag_key, validate_tag_value, validate_tags,
};
use crate::metrics::{MetricsError, Result};
use crate::metrics_data::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
const MILLISECOND_DURATION_UNIT: &str = "ms";
const MILLISECOND_DURATION_DESCRIPTION: &str = "Duration in milliseconds.";
const MILLISECOND_DURATION_BOUNDARIES: &[f64] = &[
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1_000.0, 1_250.0, 1_500.0,
    1_750.0, 2_000.0, 2_250.0, 2_500.0, 3_000.0, 3_500.0, 4_000.0, 4_500.0, 5_000.0, 6_000.0,
    7_000.0, 7_500.0, 8_000.0, 9_000.0, 10_000.0, 12_000.0, 15_000.0, 20_000.0, 30_000.0, 60_000.0,
    120_000.0,
];
const SECOND_DURATION_UNIT: &str = "s";
const SECOND_DURATION_BOUNDARIES: &[f64] = &[
    0.0, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0, 12.0,
    15.0, 20.0, 30.0, 60.0, 120.0,
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct InstrumentKey {
    kind: u8,
    name: String,
    unit: String,
    description: String,
}
#[derive(Clone, Copy)]
enum Measurement {
    Counter(u64),
    Gauge(i64),
    Histogram(f64),
}
impl Measurement {
    fn kind(self) -> u8 {
        match self {
            Self::Counter(_) => 0,
            Self::Gauge(_) => 1,
            Self::Histogram(_) => 2,
        }
    }
}
#[derive(Debug, Default)]
struct Measurements {
    metrics: BTreeMap<InstrumentKey, Metric>,
}
struct ObservableGauge {
    name: String,
    description: String,
    attributes: Vec<KeyValue>,
    observe: Arc<dyn Fn() -> i64 + Send + Sync>,
}
impl std::fmt::Debug for ObservableGauge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservableGauge")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Default)]
struct State {
    runtime: Measurements,
    export: Measurements,
    gauges: Vec<ObservableGauge>,
    shutdown: bool,
}
#[derive(Debug)]
pub(super) struct MetricsClientInner {
    state: Mutex<State>,
    exporter: InMemoryMetricExporter,
    runtime_reader: bool,
    statsig_disabled_metrics: &'static [&'static str],
    default_tags: BTreeMap<String, String>,
}
impl Measurements {
    fn update(
        &mut self,
        key: InstrumentKey,
        attributes: Vec<KeyValue>,
        value: Measurement,
        bounds: &[f64],
    ) {
        let metric = self.metrics.entry(key.clone()).or_insert_with(|| Metric {
            name: key.name,
            unit: key.unit,
            description: key.description,
            data: match value {
                Measurement::Counter(_) => AggregatedMetrics::U64(MetricData::Sum(Sum::default())),
                Measurement::Gauge(_) => AggregatedMetrics::I64(MetricData::Gauge(Sum::default())),
                _ => AggregatedMetrics::F64(MetricData::Histogram(Histogram::default())),
            },
        });
        match (&mut metric.data, value) {
            (AggregatedMetrics::U64(MetricData::Sum(sum)), Measurement::Counter(value)) => {
                if let Some(point) = sum
                    .points
                    .iter_mut()
                    .find(|point| point.attributes == attributes)
                {
                    point.value = point.value.saturating_add(value);
                } else {
                    sum.points.push(SumDataPoint { attributes, value });
                }
            }
            (AggregatedMetrics::I64(MetricData::Gauge(gauge)), Measurement::Gauge(value)) => {
                if let Some(point) = gauge
                    .points
                    .iter_mut()
                    .find(|point| point.attributes == attributes)
                {
                    point.value = value;
                } else {
                    gauge.points.push(SumDataPoint { attributes, value });
                }
            }
            (
                AggregatedMetrics::F64(MetricData::Histogram(histogram)),
                Measurement::Histogram(value),
            ) => {
                let index = histogram
                    .points
                    .iter()
                    .position(|point| point.attributes == attributes)
                    .unwrap_or_else(|| {
                        histogram.points.push(HistogramDataPoint {
                            attributes,
                            count: 0,
                            sum: 0.0,
                            bounds: bounds.to_vec(),
                            buckets: vec![0; bounds.len() + 1],
                        });
                        histogram.points.len() - 1
                    });
                let point = &mut histogram.points[index];
                point.count += 1;
                point.sum += value;
                let bucket = point.bounds.partition_point(|bound| *bound < value);
                point.buckets[bucket] += 1;
            }
            _ => {}
        }
    }
    fn snapshot(self) -> ResourceMetrics {
        ResourceMetrics {
            scopes: vec![ScopeMetrics {
                items: self.metrics.into_values().collect(),
            }],
        }
    }
}
impl MetricsClientInner {
    fn attributes(&self, tags: &[(&str, &str)]) -> Result<Vec<KeyValue>> {
        let mut merged = self.default_tags.clone();
        for (key, value) in tags {
            validate_tag_key(key)?;
            validate_tag_value(value)?;
            merged.insert((*key).into(), (*value).into());
        }
        Ok(merged
            .into_iter()
            .map(|(key, value)| KeyValue::new(key, value))
            .collect())
    }
    fn record(
        &self,
        name: &str,
        description: Option<&str>,
        unit: &str,
        value: Measurement,
        tags: &[(&str, &str)],
        bounds: &[f64],
    ) -> Result<()> {
        validate_metric_name(name)?;
        let attributes = self.attributes(tags)?;
        if self.statsig_disabled_metrics.contains(&name) {
            return Ok(());
        }
        let key = InstrumentKey {
            kind: value.kind(),
            name: name.into(),
            unit: unit.into(),
            description: description.unwrap_or_default().into(),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.shutdown {
            if self.runtime_reader {
                state
                    .runtime
                    .update(key.clone(), attributes.clone(), value, bounds);
            }
            state.export.update(key, attributes, value, bounds);
        }
        Ok(())
    }
    fn counter(
        &self,
        name: &str,
        description: Option<&str>,
        inc: i64,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        validate_metric_name(name)?;
        if inc < 0 {
            return Err(MetricsError::NegativeCounterIncrement {
                name: name.into(),
                inc,
            });
        }
        self.record(
            name,
            description,
            "",
            Measurement::Counter(inc as u64),
            tags,
            &[],
        )
    }
    fn histogram(
        &self,
        name: &str,
        value: i64,
        boundaries: Option<&[f64]>,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.record(
            name,
            None,
            "",
            Measurement::Histogram(value as f64),
            tags,
            boundaries.unwrap_or(&[
                0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0,
                5000.0, 7500.0, 10000.0,
            ]),
        )
    }
    fn gauge(
        &self,
        name: &str,
        description: Option<&str>,
        value: i64,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.record(name, description, "", Measurement::Gauge(value), tags, &[])
    }
    fn register_observable_gauge(
        &self,
        name: &str,
        description: &str,
        observe: impl Fn() -> i64 + Send + Sync + 'static,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        validate_metric_name(name)?;
        let attributes = self.attributes(tags)?;
        if !self.statsig_disabled_metrics.contains(&name) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .gauges
                .push(ObservableGauge {
                    name: name.into(),
                    description: description.into(),
                    attributes,
                    observe: Arc::new(observe),
                });
        }
        Ok(())
    }
    fn duration_histogram(
        &self,
        name: &str,
        value: f64,
        unit: &'static str,
        description: &str,
        bounds: &'static [f64],
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.record(
            name,
            Some(description),
            unit,
            Measurement::Histogram(value),
            tags,
            bounds,
        )
    }
    fn collect(&self, exporting: bool) -> ResourceMetrics {
        // Run user callbacks outside the state mutex so a callback can read other metrics.
        let gauges: Vec<_> = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gauges
            .iter()
            .map(|g| {
                (
                    g.name.clone(),
                    g.description.clone(),
                    g.attributes.clone(),
                    Arc::clone(&g.observe),
                )
            })
            .collect();
        let observations: Vec<_> = gauges
            .into_iter()
            .map(|(name, description, attrs, observe)| (name, description, attrs, observe()))
            .collect();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let measurements = if exporting {
            &mut state.export
        } else {
            &mut state.runtime
        };
        for (name, description, attrs, value) in observations {
            measurements.update(
                InstrumentKey {
                    kind: 1,
                    name,
                    unit: String::new(),
                    description,
                },
                attrs,
                Measurement::Gauge(value),
                &[],
            );
        }
        std::mem::take(measurements).snapshot()
    }
    fn shutdown(&self) -> Result<()> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutdown {
                return Ok(());
            }
            state.shutdown = true;
        }
        let snapshot = self.collect(true);
        if snapshot
            .scope_metrics()
            .any(|scope| scope.metrics().next().is_some())
        {
            self.exporter
                .finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(snapshot);
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct MetricsClient {
    pub(super) inner: Arc<MetricsClientInner>,
    pub(super) active: Option<Arc<RwLock<Arc<MetricsClientInner>>>>,
}
impl MetricsClient {
    pub fn new(config: MetricsConfig) -> Result<Self> {
        validate_tags(&config.default_tags)?;
        let MetricsExporter::InMemory(exporter) = config.exporter else {
            return Err(MetricsError::ExporterDisabled);
        };
        Ok(Self {
            inner: Arc::new(MetricsClientInner {
                state: Mutex::default(),
                exporter,
                runtime_reader: config.runtime_reader,
                statsig_disabled_metrics: config.statsig_disabled_metrics,
                default_tags: config.default_tags,
            }),
            active: None,
        })
    }
    pub(super) fn active_inner(&self) -> Arc<MetricsClientInner> {
        match &self.active {
            Some(active) => active
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            None => Arc::clone(&self.inner),
        }
    }

    /// Send a single counter increment.
    pub fn counter(&self, name: &str, inc: i64, tags: &[(&str, &str)]) -> Result<()> {
        self.active_inner()
            .counter(name, /*description*/ None, inc, tags)
    }

    /// Send a single counter increment with an instrument description.
    pub fn counter_with_description(
        &self,
        name: &str,
        description: &str,
        inc: i64,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner()
            .counter(name, Some(description), inc, tags)
    }

    /// Send a single histogram sample.
    pub fn histogram(&self, name: &str, value: i64, tags: &[(&str, &str)]) -> Result<()> {
        self.active_inner()
            .histogram(name, value, /*boundaries*/ None, tags)
    }

    /// Send a single histogram sample using explicit bucket boundaries.
    pub fn histogram_with_boundaries(
        &self,
        name: &str,
        value: i64,
        boundaries: &[f64],
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner()
            .histogram(name, value, Some(boundaries), tags)
    }

    /// Send a single gauge measurement.
    pub fn gauge(&self, name: &str, value: i64, tags: &[(&str, &str)]) -> Result<()> {
        self.active_inner()
            .gauge(name, /*description*/ None, value, tags)
    }

    /// Send a single gauge measurement with an instrument description.
    pub fn gauge_with_description(
        &self,
        name: &str,
        description: &str,
        value: i64,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner()
            .gauge(name, Some(description), value, tags)
    }

    /// Register a gauge callback that reports the current value on every collection.
    pub fn register_observable_gauge_with_description(
        &self,
        name: &str,
        description: &str,
        observe: impl Fn() -> i64 + Send + Sync + 'static,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner()
            .register_observable_gauge(name, description, observe, tags)
    }

    /// Record a duration in milliseconds using a histogram.
    pub fn record_duration(
        &self,
        name: &str,
        duration: Duration,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner().duration_histogram(
            name,
            duration.as_millis().min(i64::MAX as u128) as f64,
            MILLISECOND_DURATION_UNIT,
            MILLISECOND_DURATION_DESCRIPTION,
            MILLISECOND_DURATION_BOUNDARIES,
            tags,
        )
    }

    /// Record a duration supplied as fractional milliseconds using a histogram.
    pub(crate) fn record_duration_ms_f64(
        &self,
        name: &str,
        duration_ms: f64,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner().duration_histogram(
            name,
            duration_ms,
            MILLISECOND_DURATION_UNIT,
            MILLISECOND_DURATION_DESCRIPTION,
            MILLISECOND_DURATION_BOUNDARIES,
            tags,
        )
    }

    /// Record a duration in seconds using a histogram with an instrument description.
    pub fn record_duration_seconds_with_description(
        &self,
        name: &str,
        description: &str,
        duration: Duration,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.active_inner().duration_histogram(
            name,
            duration.as_secs_f64(),
            SECOND_DURATION_UNIT,
            description,
            SECOND_DURATION_BOUNDARIES,
            tags,
        )
    }

    pub fn start_timer(
        &self,
        name: &str,
        tags: &[(&str, &str)],
    ) -> std::result::Result<Timer, MetricsError> {
        Ok(Timer::new(name, tags, self))
    }

    pub fn snapshot(&self) -> Result<ResourceMetrics> {
        let inner = self.active_inner();
        if !inner.runtime_reader {
            return Err(MetricsError::RuntimeSnapshotUnavailable);
        }
        Ok(inner.collect(false))
    }
    pub fn shutdown(&self) -> Result<()> {
        super::buffered::GLOBAL.suspend(self);
        self.inner.shutdown()
    }
}
