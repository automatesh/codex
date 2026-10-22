//! Applies the selected server's socket policy on initial connections and reconnects.

use crate::AppServerTarget;
#[cfg(windows)]
use crate::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use crate::connect_remote_app_server;
use codex_app_server_client::AppServerClient;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerClient;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerConnectArgs;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerEndpoint;
#[cfg(windows)]
use codex_utils_absolute_path::AbsolutePathBuf;

pub(crate) async fn connect(target: &AppServerTarget) -> color_eyre::Result<AppServerClient> {
    validate_instruction_backend(
        target,
        codex_instruction_control::runtime_snapshot().is_some(),
    )?;
    match target {
        AppServerTarget::Embedded => {
            color_eyre::eyre::bail!("embedded sessions have no remote connection")
        }
        #[cfg(windows)]
        AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
            ..
        } => {
            // Revalidate at the real connection, not just the earlier discovery probe.
            let (socket_path, _directory) =
                codex_uds::validate_private_socket_path(socket_path.as_path())?;
            let app_server =
                RemoteAppServerClient::connect_local_daemon(RemoteAppServerConnectArgs {
                    endpoint: RemoteAppServerEndpoint::UnixSocket {
                        socket_path: AbsolutePathBuf::from_absolute_path_checked(socket_path)?,
                    },
                    client_name: "codex-tui".to_string(),
                    client_version: env!("CARGO_PKG_VERSION").to_string(),
                    experimental_api: true,
                    mcp_server_openai_form_elicitation: false,
                    opt_out_notification_methods: Vec::new(),
                    channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
                })
                .await?;
            Ok(AppServerClient::Remote(app_server))
        }
        AppServerTarget::LocalDaemon { endpoint, .. } | AppServerTarget::Remote { endpoint } => {
            connect_remote_app_server(endpoint.clone()).await
        }
    }
}

fn validate_instruction_backend(
    target: &AppServerTarget,
    protected: bool,
) -> color_eyre::Result<()> {
    if protected && !matches!(target, AppServerTarget::Embedded) {
        color_eyre::eyre::bail!(
            "Protected instructions cannot reconnect to an unattested app-server backend; use the embedded app server."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use codex_utils_absolute_path::AbsolutePathBuf;

    #[test]
    fn protected_reconnect_rejects_old_daemon_and_explicit_remote() {
        let endpoint = RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::relative_to_current_dir("old-daemon.sock").unwrap(),
        };
        for target in [
            AppServerTarget::Remote {
                endpoint: endpoint.clone(),
            },
            AppServerTarget::LocalDaemon {
                endpoint,
                allow_embedded_fallback: true,
            },
        ] {
            assert!(validate_instruction_backend(&target, true).is_err());
            assert!(validate_instruction_backend(&target, false).is_ok());
        }
        assert!(validate_instruction_backend(&AppServerTarget::Embedded, true).is_ok());
    }
}
