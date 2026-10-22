use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::RequestId;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::metrics_data::AggregatedMetrics;
use codex_otel::metrics_data::InMemoryMetricExporter;
use codex_otel::metrics_data::MetricData;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::timeout;

use super::ConcurrentRequestLimit;
use super::RequestDispatchMode;
use super::RequestDispatcher;
use super::RequestTaskResult;
use crate::ExecServerRuntimeOptions;
use crate::connection::JsonRpcConnectionEvent;
use crate::discover_v2::capability_manager::CapabilityManager;
use crate::rpc::RpcNotificationSender;
use crate::rpc::RpcRouter;
use crate::rpc::RpcServerOutboundMessage;
use crate::rpc::invalid_request;
use crate::server::ExecServerHandler;
use crate::server::session_registry::SessionRegistry;
use crate::telemetry::ExecServerTelemetry;

/// Public limits reject values that cannot safely enable semaphore-backed concurrency.
#[test]
fn concurrent_request_limit_rejects_invalid_values() {
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 0),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 1),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(Semaphore::MAX_PERMITS.saturating_add(1)),
        None
    );
    assert_eq!(
        ConcurrentRequestLimit::new(/*max_concurrent_requests*/ 2).map(ConcurrentRequestLimit::get),
        Some(2)
    );
}

/// CLI parsing keeps one request inline and bounds larger positive concurrency limits.
#[test]
fn request_dispatch_mode_parses_bounded_concurrency() {
    assert!(matches!("1".parse(), Ok(RequestDispatchMode::Inline)));
    assert!("0".parse::<RequestDispatchMode>().is_err());

    let oversized_limit = Semaphore::MAX_PERMITS.saturating_add(1).to_string();
    let mode = oversized_limit
        .parse::<RequestDispatchMode>()
        .expect("parse oversized concurrent request limit");
    let RequestDispatchMode::Concurrent {
        max_concurrent_requests,
    } = mode
    else {
        panic!("expected concurrent request dispatch");
    };
    assert_eq!(max_concurrent_requests.get(), Semaphore::MAX_PERMITS);
}

/// End-to-end request spans retain the wire method and inbound trace with bounded names.

/// Total request timing includes queueing without changing dispatch or queue timing.

struct RequestTelemetryFixture {
    metrics: MetricsClient,
    dispatcher: RequestDispatcher,
    outgoing_rx: mpsc::Receiver<RpcServerOutboundMessage>,
    disconnected_tx: watch::Sender<bool>,
}

fn request_telemetry_fixture(router: RpcRouter<ExecServerHandler>) -> RequestTelemetryFixture {
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-exec-server",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )
    .expect("metrics client");
    let telemetry = ExecServerTelemetry::new(metrics.clone());
    let (outgoing_tx, outgoing_rx) = mpsc::channel(/*buffer*/ 1);
    let notifications = RpcNotificationSender::new(outgoing_tx.clone());
    let requests = notifications.request_sender();
    let handler = Arc::new(ExecServerHandler::new(
        SessionRegistry::new(telemetry.clone()),
        CapabilityManager::new(crate::LocalFileSystem::default()),
        notifications,
        ExecServerRuntimeOptions::new(
            std::env::current_exe().expect("current executable"),
            /*codex_linux_sandbox_exe*/ None,
        )
        .expect("runtime paths"),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    ));
    let (disconnected_tx, disconnected_rx) = watch::channel(/*init*/ false);
    let dispatcher = RequestDispatcher::new(
        Arc::new(router),
        handler,
        outgoing_tx,
        disconnected_rx,
        requests,
        telemetry,
        RequestDispatchMode::Inline,
    );
    RequestTelemetryFixture {
        metrics,
        dispatcher,
        outgoing_rx,
        disconnected_tx,
    }
}

fn assert_request_completion(metrics: &MetricsClient, method: &str, result: &str) -> [Duration; 2] {
    let snapshot = metrics.snapshot().expect("completed request metrics");
    let metric = snapshot
        .scope_metrics()
        .flat_map(codex_otel::metrics_data::ScopeMetrics::metrics)
        .find(|metric| metric.name() == "exec_server_requests_total")
        .expect("request counter");
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
        panic!("request counter should be a u64 sum");
    };
    let points: Vec<_> = sum.data_points().collect();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].value(), 1, "record the completion only once");

    [
        "exec_server_request_duration_seconds",
        "exec_server_request_total_duration_seconds",
    ]
    .map(|name| {
        let metric = snapshot
            .scope_metrics()
            .flat_map(codex_otel::metrics_data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == name)
            .expect("request duration histogram");
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
            panic!("request duration should be an f64 histogram");
        };
        let points: Vec<_> = histogram.data_points().collect();
        assert_eq!(points.len(), 1);
        let point = points[0];
        let attributes: BTreeMap<_, _> = point
            .attributes()
            .map(|attribute| {
                (
                    attribute.key.as_str().to_string(),
                    attribute.value.as_str().to_string(),
                )
            })
            .collect();
        assert_eq!(
            attributes,
            BTreeMap::from([
                ("method".to_string(), method.to_string()),
                ("result".to_string(), result.to_string()),
            ])
        );
        assert_eq!(point.count(), 1, "record each duration only once");
        Duration::from_secs_f64(point.sum())
    })
}

/// A synthetic receipt timestamp makes the pre-dispatch boundary deterministic for every result.
#[tokio::test]
async fn total_duration_preserves_dispatch_duration_and_completion_results() {
    for (method, expected_method, expected_result, close_response) in [
        ("test/success", "test/success", "success", false),
        ("test/error", "test/error", "error", false),
        ("test/unknown", "unknown", "error", false),
        ("test/success", "test/success", "disconnected", true),
        ("test/unknown", "unknown", "disconnected", true),
    ] {
        let mut router = RpcRouter::new();
        router.request(
            "test/success",
            |_handler: Arc<ExecServerHandler>, _params: ()| async {
                Ok::<_, codex_exec_server_protocol::JSONRPCErrorError>(())
            },
        );
        router.request(
            "test/error",
            |_handler: Arc<ExecServerHandler>, _params: ()| async {
                Err::<(), _>(invalid_request("synthetic route error".to_string()))
            },
        );
        let mut fixture = request_telemetry_fixture(router);
        if close_response {
            fixture.outgoing_rx.close();
        }
        let pre_dispatch_wait = Duration::from_secs(5);
        let received_at = Instant::now() - pre_dispatch_wait;
        let result = fixture
            .dispatcher
            .dispatch_request(
                JSONRPCRequest {
                    id: RequestId::Integer(1),
                    method: method.to_string(),
                    params: None,
                    trace: None,
                },
                tracing::Span::none(),
                received_at,
            )
            .await;
        assert_eq!(
            matches!(result, RequestTaskResult::ConnectionClosed),
            close_response
        );
        let [dispatch_duration, total_duration] =
            assert_request_completion(&fixture.metrics, expected_method, expected_result);
        assert!(total_duration >= dispatch_duration + pre_dispatch_wait);
        fixture.metrics.shutdown().expect("shutdown metrics");
    }
}

/// Disconnecting a running request emits the same result and count for both duration definitions.
#[tokio::test]
async fn total_duration_records_disconnection_during_execution() {
    let execution_started = Arc::new(Notify::new());
    let notify_execution_started = Arc::clone(&execution_started);
    let mut router = RpcRouter::new();
    router.request(
        "test/pending",
        move |_handler: Arc<ExecServerHandler>, _params: ()| {
            let execution_started = Arc::clone(&notify_execution_started);
            async move {
                execution_started.notify_one();
                std::future::pending::<Result<(), codex_exec_server_protocol::JSONRPCErrorError>>()
                    .await
            }
        },
    );
    let mut fixture = request_telemetry_fixture(router);
    let pre_dispatch_wait = Duration::from_secs(5);
    let received_at = Instant::now() - pre_dispatch_wait;
    let dispatch = fixture.dispatcher.dispatch_request(
        JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "test/pending".to_string(),
            params: None,
            trace: None,
        },
        tracing::Span::none(),
        received_at,
    );
    let disconnect = async {
        execution_started.notified().await;
        fixture
            .disconnected_tx
            .send(/*value*/ true)
            .expect("disconnect request");
    };
    let (result, ()) = timeout(Duration::from_secs(1), async {
        tokio::join!(dispatch, disconnect)
    })
    .await
    .expect("disconnected request should finish");
    assert!(matches!(result, RequestTaskResult::ConnectionClosed));
    let [dispatch_duration, total_duration] =
        assert_request_completion(&fixture.metrics, "test/pending", "disconnected");
    assert!(total_duration >= dispatch_duration + pre_dispatch_wait);
    fixture.metrics.shutdown().expect("shutdown metrics");
}
