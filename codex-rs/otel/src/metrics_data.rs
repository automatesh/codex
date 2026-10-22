//! Process-local metric snapshots. These types have no exporter, worker, or network transport.
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}
impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl ToString) -> Self {
        Self {
            key: key.into(),
            value: value.to_string(),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct ResourceMetrics {
    pub(crate) scopes: Vec<ScopeMetrics>,
}
impl ResourceMetrics {
    pub fn scope_metrics(&self) -> impl Iterator<Item = &ScopeMetrics> {
        self.scopes.iter()
    }
}
#[derive(Clone, Debug, Default)]
pub struct ScopeMetrics {
    pub(crate) items: Vec<Metric>,
}
impl ScopeMetrics {
    pub fn metrics(&self) -> impl Iterator<Item = &Metric> {
        self.items.iter()
    }
}
#[derive(Clone, Debug)]
pub struct Metric {
    pub(crate) name: String,
    pub(crate) unit: String,
    pub(crate) description: String,
    pub(crate) data: AggregatedMetrics,
}
impl Metric {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn unit(&self) -> &str {
        &self.unit
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn data(&self) -> &AggregatedMetrics {
        &self.data
    }
}
#[derive(Clone, Debug)]
pub enum AggregatedMetrics {
    U64(MetricData<u64>),
    I64(MetricData<i64>),
    F64(MetricData<f64>),
}
#[derive(Clone, Debug)]
pub enum MetricData<T> {
    Sum(Sum<T>),
    Gauge(Gauge<T>),
    Histogram(Histogram<T>),
}
#[derive(Clone, Debug, Default)]
pub struct Sum<T> {
    pub(crate) points: Vec<SumDataPoint<T>>,
}
impl<T> Sum<T> {
    pub fn data_points(&self) -> impl Iterator<Item = &SumDataPoint<T>> {
        self.points.iter()
    }
}
#[derive(Clone, Debug)]
pub struct SumDataPoint<T> {
    pub(crate) attributes: Vec<KeyValue>,
    pub(crate) value: T,
}
impl<T: Copy> SumDataPoint<T> {
    pub fn value(&self) -> T {
        self.value
    }
    pub fn attributes(&self) -> impl Iterator<Item = &KeyValue> {
        self.attributes.iter()
    }
}
pub type Gauge<T> = Sum<T>;
pub type GaugeDataPoint<T> = SumDataPoint<T>;
#[derive(Clone, Debug, Default)]
pub struct Histogram<T> {
    pub(crate) points: Vec<HistogramDataPoint<T>>,
}
impl<T> Histogram<T> {
    pub fn data_points(&self) -> impl Iterator<Item = &HistogramDataPoint<T>> {
        self.points.iter()
    }
}
#[derive(Clone, Debug)]
pub struct HistogramDataPoint<T> {
    pub(crate) attributes: Vec<KeyValue>,
    pub(crate) count: u64,
    pub(crate) sum: T,
    pub(crate) bounds: Vec<f64>,
    pub(crate) buckets: Vec<u64>,
}
impl<T: Copy> HistogramDataPoint<T> {
    pub fn attributes(&self) -> impl Iterator<Item = &KeyValue> {
        self.attributes.iter()
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn sum(&self) -> T {
        self.sum
    }
    pub fn bounds(&self) -> impl Iterator<Item = f64> + '_ {
        self.bounds.iter().copied()
    }
    pub fn bucket_counts(&self) -> impl Iterator<Item = u64> + '_ {
        self.buckets.iter().copied()
    }
}
#[derive(Clone, Debug, Default)]
pub struct InMemoryMetricExporter {
    pub(crate) finished: Arc<Mutex<Vec<ResourceMetrics>>>,
}
impl InMemoryMetricExporter {
    pub fn get_finished_metrics(&self) -> std::io::Result<Vec<ResourceMetrics>> {
        Ok(self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
    pub fn reset(&self) {
        self.finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}
