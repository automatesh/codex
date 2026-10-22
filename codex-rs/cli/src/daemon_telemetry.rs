use codex_utils_cli::CliConfigOverrides;

pub(crate) async fn record_command(
    _overrides: &CliConfigOverrides,
    _analytics_default_enabled: bool,
    _target: &'static str,
    _result: &anyhow::Result<Option<codex_app_server_daemon::UpdateOutput>>,
) {
}
