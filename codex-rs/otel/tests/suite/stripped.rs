use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_otel::{
    MetricsClient, MetricsConfig, MetricsError, OtelExporter, OtelHttpProtocol, OtelProvider,
    OtelSettings,
};
use std::io::ErrorKind;
use std::net::TcpListener;

#[test]
fn exporter_configuration_cannot_enable_network_telemetry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let exporters = [
        OtelExporter::None,
        OtelExporter::Statsig,
        OtelExporter::OtlpGrpc {
            endpoint: endpoint.clone(),
            headers: Default::default(),
            tls: None,
        },
        OtelExporter::OtlpHttp {
            endpoint: endpoint.clone(),
            headers: Default::default(),
            protocol: OtelHttpProtocol::Binary,
            tls: None,
        },
        OtelExporter::OtlpHttp {
            endpoint,
            headers: Default::default(),
            protocol: OtelHttpProtocol::Json,
            tls: None,
        },
    ];
    for exporter in exporters {
        let settings = OtelSettings {
            http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            environment: "test".into(),
            service_name: "test".into(),
            service_version: "1".into(),
            codex_home: ".".into(),
            exporter: exporter.clone(),
            trace_exporter: exporter.clone(),
            metrics_exporter: exporter.clone(),
            runtime_metrics: true,
            span_attributes: Default::default(),
            tracestate: Default::default(),
        };
        assert!(OtelProvider::try_new(&settings).unwrap().is_none());
        assert!(matches!(
            MetricsClient::new(MetricsConfig::otlp("test", "test", "1", exporter)),
            Err(MetricsError::ExporterDisabled)
        ));
    }
    assert!(codex_otel::global_statsig_metrics_settings().is_none());
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}
