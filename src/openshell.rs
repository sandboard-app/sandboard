//! In-process OpenShell gateway client (gRPC + mTLS or OIDC).
//!
//! One place that knows the gateway surface, so the supervisor never builds an
//! argv and never shells out. Auth comes from the sealed Settings bundle
//! (mTLS PEMs or OIDC tokens); the endpoint is board state. See `docs/sandbox.md`.
//!
//! **Everything here takes a timeout, and that is not defensive style.** Every
//! failure mode observed in phase 0 — blocked metadata server, denied egress,
//! git waiting on a credential prompt — presented as a *hang*, not an error. A
//! call without a deadline is a supervisor that stops making progress and
//! never says why.

use crate::model::OpenShellOidcConfig;
use crate::secrets::{OpenShellMtlsBundle, OpenShellOidcBundle};
use futures::StreamExt;
use openshell_core::auth::EdgeAuthInterceptor;
use openshell_core::metadata::{ObjectId, ObjectLabels, ObjectName};
use openshell_core::proto::open_shell_client::OpenShellClient;
use openshell_core::proto::datamodel::v1::{ObjectMeta, Provider};
use openshell_core::proto::{
    AttachSandboxProviderRequest, ConfigureProviderRefreshRequest, CreateProviderRequest,
    CreateSandboxRequest, DeleteProviderProfileRequest, DeleteProviderRequest,
    DeleteSandboxRequest, DetachSandboxProviderRequest, ExecSandboxEvent, ExecSandboxInput,
    ExecSandboxRequest, ExecSandboxWindowResize, GetSandboxLogsRequest, GetSandboxRequest,
    HealthRequest, ImportProviderProfilesRequest, ListProviderProfilesRequest,
    ListProvidersRequest, ListSandboxesRequest, ProviderCredentialRefreshStrategy,
    ProviderProfile as ProtoProviderProfile, ProviderProfileImportItem, SandboxPhase,
    SandboxSpec as ProtoSandboxSpec, SandboxTemplate, ServiceStatus,
    UpdateProviderProfilesRequest, UpdateProviderRequest, exec_sandbox_event, exec_sandbox_input,
};
use rand::Rng;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use prost_types::{Struct, Value, value::Kind};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// Skew matching OpenShell CLI `is_token_expired` (refresh within 30s of expiry).
const OIDC_EXPIRY_SKEW_SECS: u64 = 30;

type OsClient = OpenShellClient<InterceptedService<Channel, EdgeAuthInterceptor>>;

/// How this client authenticates. Selected explicitly in Settings — not inferred.
#[derive(Clone)]
pub enum GatewayAuth {
    Mtls(OpenShellMtlsBundle),
    Oidc {
        config: OpenShellOidcConfig,
        tokens: Arc<tokio::sync::Mutex<OpenShellOidcBundle>>,
        on_refresh: Option<Arc<dyn Fn(OpenShellOidcBundle) + Send + Sync>>,
        /// Optional server CA (e.g. leftover local-gateway PEMs). OpenShell CLI
        /// also pins mTLS CA material when present alongside OIDC Bearer auth.
        server_ca_pem: Option<String>,
    },
    /// Mode is OIDC but tokens are missing — `configured()` fails with a login hint.
    OidcIncomplete {
        config: OpenShellOidcConfig,
    },
}

impl GatewayAuth {
    pub fn oidc(
        config: OpenShellOidcConfig,
        bundle: OpenShellOidcBundle,
        on_refresh: Option<Arc<dyn Fn(OpenShellOidcBundle) + Send + Sync>>,
        server_ca_pem: Option<String>,
    ) -> Self {
        Self::Oidc {
            config,
            tokens: Arc::new(tokio::sync::Mutex::new(bundle)),
            on_refresh,
            server_ca_pem,
        }
    }
}

impl std::fmt::Debug for GatewayAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mtls(_) => f.write_str("GatewayAuth::Mtls"),
            Self::Oidc {
                config,
                server_ca_pem,
                ..
            } => f
                .debug_struct("GatewayAuth::Oidc")
                .field("issuer", &config.issuer)
                .field("client_id", &config.client_id)
                .field("has_server_ca", &server_ca_pem.is_some())
                .finish_non_exhaustive(),
            Self::OidcIncomplete { config } => f
                .debug_struct("GatewayAuth::OidcIncomplete")
                .field("issuer", &config.issuer)
                .field("client_id", &config.client_id)
                .finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("openshell {op} timed out after {secs}s")]
    Timeout { op: String, secs: u64 },
    #[error("openshell {op}: {message}")]
    Failed { op: String, message: String },
    #[error("openshell not configured: {0}")]
    NotConfigured(String),
    #[error("openshell connect: {0}")]
    Connect(String),
    #[error("openshell policy: {0}")]
    Policy(String),
    #[error("openshell io: {0}")]
    Io(#[source] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Outcome of a gateway health probe for Settings → OpenShell and cockpit surfaces.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct GatewayStatus {
    pub healthy: bool,
    /// Short human summary.
    pub summary: String,
    /// True when endpoint or selected auth material is missing (Settings incomplete).
    pub not_configured: bool,
    /// Optional detail when unhealthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One sandbox, as the gateway reports it. Deliberately partial: unknown
/// fields are ignored so a gateway that grows a field doesn't break us.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Sandbox {
    pub name: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub id: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Sandbox {
    /// The work item this sandbox belongs to, from the `sandboard.item` label.
    pub fn item_id(&self) -> Option<u64> {
        self.labels.get(LABEL_ITEM)?.parse().ok()
    }

    /// Control-plane cockpit sandbox (`sandboard.cockpit=1`), not a card worker.
    pub fn is_cockpit(&self) -> bool {
        self.labels
            .get(LABEL_COCKPIT)
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }
}

/// How a sandbox is created. Mirrors the flags proven in phase 0.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub name: String,
    /// OCI image reference (`sandboard-sandbox:latest`). Same semantics as CLI `--from`.
    pub from: String,
    /// Provider names to attach (from sandboard desired providers with attach=true).
    pub providers: Vec<String>,
    /// Inline OpenShell policy YAML.
    pub policy: Option<String>,
    pub env: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

/// Gateway provider record (secrets never included — gateway omits values on list).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GatewayProvider {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub credential_keys: Vec<String>,
    pub config_keys: Vec<String>,
}

/// One event from an interactive (`ExecSandboxInteractive`) attach stream.
#[derive(Debug, Clone)]
pub enum InteractiveEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

/// Live interactive exec — stdin/resize in, stdout/stderr/exit out.
/// Dropping this closes the input side; the gateway stream ends on Exit or error.
pub struct InteractiveExec {
    input_tx: mpsc::Sender<ExecSandboxInput>,
    events: mpsc::Receiver<InteractiveEvent>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl InteractiveExec {
    pub async fn write_stdin(&self, data: Vec<u8>) -> Result<()> {
        self.input_tx
            .send(ExecSandboxInput {
                payload: Some(exec_sandbox_input::Payload::Stdin(data)),
            })
            .await
            .map_err(|_| Error::Failed {
                op: "exec interactive".into(),
                message: "stdin channel closed".into(),
            })
    }

    pub async fn resize(&self, cols: u32, rows: u32) -> Result<()> {
        self.input_tx
            .send(ExecSandboxInput {
                payload: Some(exec_sandbox_input::Payload::Resize(ExecSandboxWindowResize {
                    cols,
                    rows,
                })),
            })
            .await
            .map_err(|_| Error::Failed {
                op: "exec interactive".into(),
                message: "resize channel closed".into(),
            })
    }

    pub async fn next_event(&mut self) -> Option<InteractiveEvent> {
        self.events.recv().await
    }
}

impl Drop for InteractiveExec {
    fn drop(&mut self) {
        // Dropping input_tx closes the gRPC client stream.
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

/// Provider type profile for Settings form scaffolding.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderTypeProfile {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub category: String,
    pub credential_env_vars: Vec<String>,
    pub config_keys: Vec<String>,
}

/// Refresh bootstrap applied after CreateProvider (gcloud ADC, etc.).
#[derive(Debug, Clone)]
pub struct ProviderRefreshSpec {
    pub credential_key: String,
    pub strategy: String,
    pub material: BTreeMap<String, String>,
    pub secret_material_keys: Vec<String>,
}

/// What a finished command produced.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

pub const LABEL_ITEM: &str = "sandboard.item";
/// Marks the durable control-plane cockpit sandbox (not a card worker).
pub const LABEL_COCKPIT: &str = "sandboard.cockpit";

#[cfg(test)]
type MockHandler = std::sync::Arc<dyn Fn(&[String]) -> Output + Send + Sync>;

#[derive(Clone)]
pub struct OpenShell {
    endpoint: Option<String>,
    auth: Option<GatewayAuth>,
    /// Applies to control-plane calls (create, list, delete). Exec carries its
    /// own, because an agent legitimately runs for minutes.
    default_timeout: Duration,
    /// In-process stand-in for unit tests. Receives a synthesized argv-shaped
    /// slice so existing supervisor/store mocks keep working without a gateway.
    #[cfg(test)]
    mock: Option<MockHandler>,
}

impl std::fmt::Debug for OpenShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenShell")
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .field("default_timeout", &self.default_timeout)
            .finish()
    }
}

impl Default for OpenShell {
    fn default() -> Self {
        Self {
            endpoint: None,
            auth: None,
            default_timeout: Duration::from_secs(120),
            #[cfg(test)]
            mock: None,
        }
    }
}

impl OpenShell {
    pub fn new(
        endpoint: Option<String>,
        auth: Option<GatewayAuth>,
        default_timeout: Duration,
    ) -> Self {
        Self {
            endpoint: endpoint
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            auth,
            default_timeout,
            #[cfg(test)]
            mock: None,
        }
    }

    /// In-process stand-in — no network. Handler sees argv-shaped calls
    /// (`sandbox exec …`, `sandbox delete …`) matching the old CLI mock surface.
    #[cfg(test)]
    pub fn mock(
        handler: impl Fn(&[String]) -> Output + Send + Sync + 'static,
        default_timeout: Duration,
    ) -> Self {
        Self {
            endpoint: Some("mock://openshell".into()),
            auth: None,
            default_timeout,
            mock: Some(std::sync::Arc::new(handler)),
        }
    }

    fn configured(&self) -> Result<()> {
        let endpoint = self.endpoint.as_deref().ok_or_else(|| {
            Error::NotConfigured("set gateway endpoint in Settings → OpenShell".into())
        })?;
        #[cfg(test)]
        if self.mock.is_some() {
            return Ok(());
        }
        if endpoint.starts_with("http://") {
            return Err(Error::NotConfigured(
                "gateway endpoint must be https:// (plaintext HTTP is not supported)".into(),
            ));
        }
        if !endpoint.starts_with("https://") {
            return Err(Error::NotConfigured(
                "gateway endpoint must be an https:// URL".into(),
            ));
        }
        match &self.auth {
            Some(GatewayAuth::Mtls(b)) if mtls_bundle_complete(b) => Ok(()),
            Some(GatewayAuth::Mtls(_)) => Err(Error::NotConfigured(
                "paste mTLS PEMs in Settings → OpenShell (auth mode: mTLS)".into(),
            )),
            Some(GatewayAuth::Oidc { config, .. }) if config.is_complete() => Ok(()),
            Some(GatewayAuth::Oidc { .. }) | Some(GatewayAuth::OidcIncomplete { .. }) => {
                Err(Error::NotConfigured(
                    "set OIDC issuer/client and Log in (Settings → OpenShell, auth mode: OIDC)"
                        .into(),
                ))
            }
            None => Err(Error::NotConfigured(
                "pick auth mode (mTLS or OIDC) in Settings → OpenShell".into(),
            )),
        }
    }

    async fn oidc_access_token(&self) -> Result<String> {
        let Some(GatewayAuth::Oidc {
            config,
            tokens,
            on_refresh,
            ..
        }) = &self.auth
        else {
            return Err(Error::NotConfigured(
                "OIDC auth is not configured".into(),
            ));
        };
        if !config.is_complete() {
            return Err(Error::NotConfigured(
                "set OIDC issuer and client id in Settings → OpenShell".into(),
            ));
        }
        let mut guard = tokens.lock().await;
        if !guard.access_expiring_soon(OIDC_EXPIRY_SKEW_SECS) {
            return Ok(guard.access_token.clone());
        }
        let input = openshell_sdk::oidc::RefreshTokenInput::new(
            guard.refresh_token.clone(),
            config.issuer.clone(),
            config.client_id.clone(),
        );
        let refreshed = openshell_sdk::oidc::refresh_token(&input)
            .await
            .map_err(|e| Error::Connect(format!("OIDC refresh: {e}")))?;
        guard.access_token = refreshed.access_token;
        if let Some(rt) = refreshed.refresh_token {
            guard.refresh_token = rt;
        }
        if let Some(exp) = refreshed.expires_at {
            guard.expires_at = exp;
        } else {
            // Provider omitted expiry — assume one hour so we still refresh.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            guard.expires_at = now.saturating_add(3600);
        }
        let next = guard.clone();
        drop(guard);
        if let Some(persist) = on_refresh {
            persist(next.clone());
        }
        Ok(next.access_token)
    }

    async fn connect(&self) -> Result<OsClient> {
        self.configured()?;
        #[cfg(test)]
        if self.mock.is_some() {
            return Err(Error::Failed {
                op: "connect".into(),
                message: "mock client has no gRPC channel".into(),
            });
        }
        let endpoint = self.endpoint.as_deref().unwrap();
        let (tls, interceptor) = match &self.auth {
            Some(GatewayAuth::Mtls(mtls)) => {
                let tls = ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(mtls.ca_pem.as_bytes()))
                    .identity(Identity::from_pem(
                        mtls.client_cert_pem.as_bytes(),
                        mtls.client_key_pem.as_bytes(),
                    ));
                let interceptor = EdgeAuthInterceptor::noop();
                (tls, interceptor)
            }
            Some(GatewayAuth::Oidc { server_ca_pem, .. }) => {
                let access = self.oidc_access_token().await?;
                let interceptor = EdgeAuthInterceptor::new(Some(&access), None).map_err(|e| {
                    Error::Connect(format!("OIDC bearer interceptor: {e}"))
                })?;
                // System/webpki roots + Let's Encrypt Generation Y roots.
                // Gen Y (ISRG Root YE/YR) is not in Mozilla/OS stores yet.
                // Optional server_ca_pem covers private-CA gateways (local
                // OpenShell) the same way the CLI pins mTLS CA with OIDC.
                let mut tls = ClientTlsConfig::new()
                    .with_enabled_roots()
                    .ca_certificate(Certificate::from_pem(LETS_ENCRYPT_GEN_Y_ROOTS.as_bytes()));
                if let Some(ca) = server_ca_pem.as_ref().filter(|s| !s.trim().is_empty()) {
                    tls = tls.ca_certificate(Certificate::from_pem(ca.as_bytes()));
                }
                if let Some(host) = tls_domain_name(endpoint) {
                    tls = tls.domain_name(host);
                }
                (tls, interceptor)
            }
            _ => {
                return Err(Error::NotConfigured(
                    "pick auth mode (mTLS or OIDC) in Settings → OpenShell".into(),
                ));
            }
        };
        let channel = Endpoint::from_shared(endpoint.to_string())
            .map_err(|e| Error::Connect(format!("invalid gateway URL: {e}")))?
            .connect_timeout(Duration::from_secs(10))
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .tls_config(tls)
            .map_err(|e| Error::Connect(format!("tls config: {e}")))?
            .connect()
            .await
            .map_err(|e| Error::Connect(format_transport_error(&e)))?;
        Ok(OpenShellClient::with_interceptor(channel, interceptor))
    }

    async fn with_timeout<T, F, Fut>(&self, op: &str, timeout: Duration, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        match tokio::time::timeout(timeout, f()).await {
            Ok(r) => r,
            Err(_) => Err(Error::Timeout {
                op: op.into(),
                secs: timeout.as_secs(),
            }),
        }
    }

    // -------------------------------------------------------- the verbs

    /// Is the gateway reachable? Cheap enough to call before claiming a card,
    /// and worth it: the compute driver stops on its own.
    pub async fn healthy(&self) -> bool {
        self.gateway_status().await.healthy
    }

    /// Probe gateway Health over mTLS for Settings / cockpit.
    pub async fn gateway_status(&self) -> GatewayStatus {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["status".into()]);
            return if out.ok() {
                GatewayStatus {
                    healthy: true,
                    summary: if out.stdout.trim().is_empty() {
                        "Connected".into()
                    } else {
                        out.stdout.trim().chars().take(2000).collect()
                    },
                    not_configured: false,
                    error: None,
                }
            } else {
                let summary = if !out.stderr.trim().is_empty() {
                    out.stderr.trim().chars().take(2000).collect()
                } else {
                    format!("health check exited {}", out.code)
                };
                GatewayStatus {
                    healthy: false,
                    summary,
                    not_configured: false,
                    error: Some(format!("health exited {}", out.code)),
                }
            };
        }

        if let Err(e) = self.configured() {
            return GatewayStatus {
                healthy: false,
                summary: e.to_string(),
                not_configured: true,
                error: Some(e.to_string()),
            };
        }

        match self
            .with_timeout("health", Duration::from_secs(15), || async {
                let mut client = self.connect().await?;
                let resp = client
                    .health(HealthRequest {})
                    .await
                    .map_err(|e| Error::Failed {
                        op: "health".into(),
                        message: e.to_string(),
                    })?;
                Ok(resp.into_inner())
            })
            .await
        {
            Ok(h) => {
                let status = ServiceStatus::try_from(h.status).unwrap_or(ServiceStatus::Unspecified);
                let healthy = status == ServiceStatus::Healthy;
                let summary = if h.version.is_empty() {
                    format!("{status:?}")
                } else {
                    format!("{status:?} (gateway {})", h.version)
                };
                GatewayStatus {
                    healthy,
                    summary,
                    not_configured: false,
                    error: (!healthy).then(|| format!("service status {status:?}")),
                }
            }
            Err(e) => GatewayStatus {
                healthy: false,
                summary: e.to_string(),
                not_configured: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn list(&self) -> Result<Vec<Sandbox>> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["sandbox".into(), "list".into(), "-o".into(), "json".into()]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox list".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return serde_json::from_str(&out.stdout).map_err(|e| Error::Failed {
                op: "sandbox list".into(),
                message: e.to_string(),
            });
        }

        self.with_timeout("sandbox list", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .list_sandboxes(ListSandboxesRequest {
                    limit: 0,
                    offset: 0,
                    label_selector: String::new(),
                    workspace: String::new(),
                    all_workspaces: false,
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox list".into(),
                    message: e.to_string(),
                })?;
            Ok(resp
                .into_inner()
                .sandboxes
                .into_iter()
                .map(|s| Sandbox {
                    name: s.object_name().to_string(),
                    id: {
                        let id = s.object_id();
                        if id.is_empty() {
                            None
                        } else {
                            Some(id.to_string())
                        }
                    },
                    // CLI/JSON use Ready/Error/… — not the raw prost i32 Debug ("2").
                    // Supervisor readiness polls this string.
                    phase: Some(phase_label(s.phase())),
                    labels: s
                        .object_labels()
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                })
                .collect())
        })
        .await
    }

    /// Sandboxes this sandboard created, keyed by work item.
    pub async fn list_ours(&self) -> Result<Vec<Sandbox>> {
        Ok(self.list().await?.into_iter().filter(|s| s.item_id().is_some()).collect())
    }

    /// Cockpit sandboxes (`sandboard.cockpit`), distinct from card `list_ours`.
    pub async fn list_cockpit(&self) -> Result<Vec<Sandbox>> {
        Ok(self.list().await?.into_iter().filter(|s| s.is_cockpit()).collect())
    }

    /// Create and wait until Ready. We exec into it afterwards.
    pub async fn create(&self, spec: &SandboxSpec) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let args = mock_create_args(spec);
            let out = mock(&args);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox create".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        let request = build_create_request(spec)?;
        self.with_timeout("sandbox create", Duration::from_secs(300), || async {
            let mut client = self.connect().await?;
            client
                .create_sandbox(request)
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox create".into(),
                    message: e.to_string(),
                })?;
            wait_ready(&mut client, &spec.name, Duration::from_secs(240)).await
        })
        .await
    }

    pub async fn list_providers(&self) -> Result<Vec<GatewayProvider>> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["provider".into(), "list".into(), "-o".into(), "json".into()]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider list".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            if out.stdout.trim().is_empty() {
                return Ok(Vec::new());
            }
            return serde_json::from_str::<Vec<GatewayProvider>>(out.stdout.trim()).map_err(|e| {
                Error::Failed {
                    op: "provider list".into(),
                    message: format!("parse mock json: {e}"),
                }
            });
        }

        self.with_timeout("provider list", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .list_providers(ListProvidersRequest {
                    limit: 0,
                    offset: 0,
                    workspace: String::new(),
                    all_workspaces: false,
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider list".into(),
                    message: e.to_string(),
                })?;
            Ok(resp
                .into_inner()
                .providers
                .into_iter()
                .map(gateway_provider_from_proto)
                .collect())
        })
        .await
    }

    pub async fn create_provider(
        &self,
        name: &str,
        provider_type: &str,
        credentials: BTreeMap<String, String>,
        config: BTreeMap<String, String>,
    ) -> Result<GatewayProvider> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "create".into(),
                "--name".into(),
                name.into(),
                "--type".into(),
                provider_type.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider create".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(GatewayProvider {
                name: name.to_string(),
                provider_type: provider_type.to_string(),
                credential_keys: credentials.keys().cloned().collect(),
                config_keys: config.keys().cloned().collect(),
            });
        }

        self.with_timeout("provider create", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .create_provider(CreateProviderRequest {
                    provider: Some(Provider {
                        metadata: Some(ObjectMeta {
                            name: name.to_string(),
                            ..Default::default()
                        }),
                        r#type: provider_type.to_string(),
                        credentials: credentials.clone().into_iter().collect(),
                        config: config.clone().into_iter().collect(),
                        credential_expires_at_ms: Default::default(),
                        profile_workspace: String::new(),
                        credential_handles: Default::default(),
                    }),
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider create".into(),
                    message: e.to_string(),
                })?;
            let p = resp.into_inner().provider.ok_or_else(|| Error::Failed {
                op: "provider create".into(),
                message: "empty provider in response".into(),
            })?;
            Ok(gateway_provider_from_proto(p))
        })
        .await
    }

    pub async fn update_provider(
        &self,
        name: &str,
        provider_type: &str,
        credentials: BTreeMap<String, String>,
        config: BTreeMap<String, String>,
    ) -> Result<GatewayProvider> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "update".into(),
                name.into(),
                "--type".into(),
                provider_type.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider update".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(GatewayProvider {
                name: name.to_string(),
                provider_type: provider_type.to_string(),
                credential_keys: credentials.keys().cloned().collect(),
                config_keys: config.keys().cloned().collect(),
            });
        }

        self.with_timeout("provider update", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .update_provider(UpdateProviderRequest {
                    provider: Some(Provider {
                        metadata: Some(ObjectMeta {
                            name: name.to_string(),
                            ..Default::default()
                        }),
                        r#type: provider_type.to_string(),
                        credentials: credentials.clone().into_iter().collect(),
                        config: config.clone().into_iter().collect(),
                        credential_expires_at_ms: Default::default(),
                        profile_workspace: String::new(),
                        credential_handles: Default::default(),
                    }),
                    credential_expires_at_ms: Default::default(),
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider update".into(),
                    message: e.to_string(),
                })?;
            let p = resp.into_inner().provider.ok_or_else(|| Error::Failed {
                op: "provider update".into(),
                message: "empty provider in response".into(),
            })?;
            Ok(gateway_provider_from_proto(p))
        })
        .await
    }

    pub async fn delete_provider(&self, name: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["provider".into(), "delete".into(), name.into()]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider delete".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        self.with_timeout("provider delete", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let _ = client
                .delete_provider(DeleteProviderRequest {
                    name: name.to_string(),
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider delete".into(),
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .await
    }

    pub async fn list_provider_profiles(&self) -> Result<Vec<ProviderTypeProfile>> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["provider".into(), "list-profiles".into(), "-o".into(), "json".into()]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider list-profiles".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            if out.stdout.trim().is_empty() {
                return Ok(Vec::new());
            }
            return serde_json::from_str(out.stdout.trim()).map_err(|e| Error::Failed {
                op: "provider list-profiles".into(),
                message: format!("parse mock json: {e}"),
            });
        }

        self.with_timeout("provider list-profiles", self.default_timeout, || async {
            let mut client = self.connect().await?;
            // Empty workspace resolves to a scope that omits workspace-imported
            // custom types (e.g. `antigravity`). Match CLI default + import.
            let resp = client
                .list_provider_profiles(ListProviderProfilesRequest {
                    limit: 0,
                    offset: 0,
                    workspace: "default".into(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider list-profiles".into(),
                    message: e.to_string(),
                })?;
            Ok(resp
                .into_inner()
                .profiles
                .into_iter()
                .map(provider_type_profile_from_proto)
                .collect())
        })
        .await
    }

    /// Import a custom provider type from YAML (OpenShell `ImportProviderProfiles`).
    ///
    /// Workspace-scoped (`default`) so it matches CLI `provider profile import`
    /// without `--global`. Callers should no-op when the type id already exists.
    pub async fn import_provider_type_yaml(&self, source: &str, yaml: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "profile".into(),
                "import".into(),
                source.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider profile import".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        let dto = openshell_providers::parse_profile_yaml(yaml).map_err(|e| Error::Failed {
            op: "provider profile import".into(),
            message: format!("parse {source}: {e}"),
        })?;
        let item = ProviderProfileImportItem {
            profile: Some(dto.to_proto()),
            source: source.to_string(),
        };
        self.with_timeout("provider profile import", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .import_provider_profiles(ImportProviderProfilesRequest {
                    profiles: vec![item.clone()],
                    workspace: "default".into(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider profile import".into(),
                    message: e.to_string(),
                })?;
            let inner = resp.into_inner();
            if inner.imported {
                return Ok(());
            }
            let diag = inner
                .diagnostics
                .into_iter()
                .map(|d| format!("{}: {}", d.field, d.message))
                .collect::<Vec<_>>()
                .join("; ");
            Err(Error::Failed {
                op: "provider profile import".into(),
                message: if diag.is_empty() {
                    "import rejected".into()
                } else {
                    diag
                },
            })
        })
        .await
    }

    /// Delete a custom provider type profile from the gateway.
    pub async fn delete_provider_type(&self, id: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "profile".into(),
                "delete".into(),
                id.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider profile delete".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        self.with_timeout("provider profile delete", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .delete_provider_profile(DeleteProviderProfileRequest {
                    id: id.to_string(),
                    workspace: "default".into(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider profile delete".into(),
                    message: e.to_string(),
                })?;
            if !resp.into_inner().deleted {
                return Err(Error::Failed {
                    op: "provider profile delete".into(),
                    message: format!("profile {id:?} not deleted"),
                });
            }
            Ok(())
        })
        .await
    }

    /// Current `resource_version` for a workspace custom provider type, if present.
    async fn provider_type_resource_version(&self, id: &str) -> Result<Option<u64>> {
        #[cfg(test)]
        if self.mock.is_some() {
            return Ok(Some(1));
        }

        self.with_timeout("provider profile version", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .list_provider_profiles(ListProviderProfilesRequest {
                    limit: 0,
                    offset: 0,
                    workspace: "default".into(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider profile version".into(),
                    message: e.to_string(),
                })?;
            Ok(resp
                .into_inner()
                .profiles
                .into_iter()
                .find(|p| p.id == id)
                .map(|p| p.resource_version))
        })
        .await
    }

    /// Update an existing custom provider type (OpenShell `UpdateProviderProfiles`).
    pub async fn update_provider_type_yaml(
        &self,
        id: &str,
        yaml: &str,
        expected_resource_version: u64,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "profile".into(),
                "update".into(),
                id.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider profile update".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        if expected_resource_version == 0 {
            return Err(Error::Failed {
                op: "provider profile update".into(),
                message: "expected_resource_version must be non-zero".into(),
            });
        }

        let mut dto = openshell_providers::parse_profile_yaml(yaml).map_err(|e| Error::Failed {
            op: "provider profile update".into(),
            message: format!("parse {id}: {e}"),
        })?;
        if dto.id.trim() != id {
            return Err(Error::Failed {
                op: "provider profile update".into(),
                message: format!("yaml id {:?} does not match {id:?}", dto.id),
            });
        }
        dto.resource_version = expected_resource_version;
        let item = ProviderProfileImportItem {
            profile: Some(dto.to_proto()),
            source: id.to_string(),
        };
        self.with_timeout("provider profile update", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let resp = client
                .update_provider_profiles(UpdateProviderProfilesRequest {
                    profile: Some(item),
                    expected_resource_version,
                    id: id.to_string(),
                    workspace: "default".into(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider profile update".into(),
                    message: e.to_string(),
                })?;
            let inner = resp.into_inner();
            if inner.updated {
                return Ok(());
            }
            let diag = inner
                .diagnostics
                .into_iter()
                .map(|d| format!("{}: {}", d.field, d.message))
                .collect::<Vec<_>>()
                .join("; ");
            Err(Error::Failed {
                op: "provider profile update".into(),
                message: if diag.is_empty() {
                    "update rejected".into()
                } else {
                    diag
                },
            })
        })
        .await
    }

    /// Import a provider type, or update it when the gateway already has that id.
    ///
    /// Delete+reimport is wrong here: an existing provider instance pins the type,
    /// so delete is ignored and the second import fails with "already exists".
    pub async fn upsert_provider_type_yaml(&self, id: &str, yaml: &str) -> Result<()> {
        match self.import_provider_type_yaml(id, yaml).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if !msg.to_ascii_lowercase().contains("already exists") {
                    return Err(e);
                }
                let Some(rv) = self.provider_type_resource_version(id).await? else {
                    return Err(Error::Failed {
                        op: "provider profile upsert".into(),
                        message: format!(
                            "profile {id:?} reports already exists but is missing from list"
                        ),
                    });
                };
                self.update_provider_type_yaml(id, yaml, rv).await
            }
        }
    }

    /// Attach a provider instance to a running sandbox (Providers v2).
    pub async fn attach_sandbox_provider(&self, sandbox: &str, provider: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "sandbox".into(),
                "provider".into(),
                "attach".into(),
                sandbox.into(),
                provider.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox provider attach".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        self.with_timeout("sandbox provider attach", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let _ = client
                .attach_sandbox_provider(AttachSandboxProviderRequest {
                    sandbox_name: sandbox.to_string(),
                    provider_name: provider.to_string(),
                    expected_resource_version: 0,
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox provider attach".into(),
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .await
    }

    /// Detach a provider instance from a running sandbox.
    #[allow(dead_code)]
    pub async fn detach_sandbox_provider(&self, sandbox: &str, provider: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "sandbox".into(),
                "provider".into(),
                "detach".into(),
                sandbox.into(),
                provider.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox provider detach".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        self.with_timeout("sandbox provider detach", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let _ = client
                .detach_sandbox_provider(DetachSandboxProviderRequest {
                    sandbox_name: sandbox.to_string(),
                    provider_name: provider.to_string(),
                    expected_resource_version: 0,
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox provider detach".into(),
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .await
    }

    pub async fn configure_provider_refresh(
        &self,
        provider: &str,
        refresh: &ProviderRefreshSpec,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "provider".into(),
                "refresh".into(),
                "configure".into(),
                provider.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "provider refresh configure".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        let strategy = refresh_strategy_from_name(&refresh.strategy)?;
        self.with_timeout("provider refresh configure", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let _ = client
                .configure_provider_refresh(ConfigureProviderRefreshRequest {
                    provider: provider.to_string(),
                    credential_key: refresh.credential_key.clone(),
                    strategy: strategy as i32,
                    material: refresh.material.clone().into_iter().collect(),
                    secret_material_keys: refresh.secret_material_keys.clone(),
                    expires_at_ms: None,
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "provider refresh configure".into(),
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .await
    }

    /// Create-or-update a provider on the gateway, then apply refresh if given.
    pub async fn apply_provider(
        &self,
        name: &str,
        provider_type: &str,
        credentials: BTreeMap<String, String>,
        config: BTreeMap<String, String>,
        refresh: Option<&ProviderRefreshSpec>,
    ) -> Result<GatewayProvider> {
        let existing = self.list_providers().await.unwrap_or_default();
        let on_gateway = existing.iter().any(|p| p.name == name);
        let gw = if on_gateway {
            self.update_provider(name, provider_type, credentials, config)
                .await?
        } else {
            self.create_provider(name, provider_type, credentials, config)
                .await?
        };
        if let Some(r) = refresh {
            self.configure_provider_refresh(name, r).await?;
        }
        Ok(gw)
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["sandbox".into(), "delete".into(), name.into()]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox delete".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        self.with_timeout("sandbox delete", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let _ = client
                .delete_sandbox(DeleteSandboxRequest {
                    name: name.to_string(),
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox delete".into(),
                    message: e.to_string(),
                })?;
            Ok(())
        })
        .await
    }

    pub async fn upload(&self, name: &str, local: &str, dest: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "sandbox".into(),
                "upload".into(),
                name.into(),
                local.into(),
                dest.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox upload".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        let local_path = PathBuf::from(local);
        let (dest_dir, tar_name) = upload_dest_parts(&local_path, dest)?;
        let archive = build_upload_tar(&local_path, &tar_name)?;
        let script = format!(
            "mkdir -p {dest} && tar xf - -C {dest}",
            dest = shell_single_quote(&dest_dir)
        );
        let out = self
            .exec_with_stdin(name, &script, archive, self.default_timeout)
            .await?;
        if !out.ok() {
            return Err(Error::Failed {
                op: "sandbox upload".into(),
                message: out.stderr.trim().to_string(),
            });
        }
        Ok(())
    }

    /// Download a file from a sandbox to the host (verdict file protocol).
    pub async fn download(&self, name: &str, remote: &str, dest: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&[
                "sandbox".into(),
                "download".into(),
                name.into(),
                remote.into(),
                dest.into(),
            ]);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "sandbox download".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            return Ok(());
        }

        let (parent, base) = split_sandbox_path(remote);
        let script = format!(
            "tar cf - -C {parent} {base}",
            parent = shell_single_quote(parent),
            base = shell_single_quote(base)
        );
        // Raw bytes — never route tar through Output.stdout (UTF-8 lossy).
        // USTAR headers contain NULs; from_utf8_lossy replaces them and the
        // archive checksum fails ("archive header checksum mismatch").
        let (code, stdout, stderr) = self
            .exec_capture(name, &script, Vec::new(), self.default_timeout)
            .await?;
        if code != 0 {
            return Err(Error::Failed {
                op: "sandbox download".into(),
                message: String::from_utf8_lossy(&stderr).trim().to_string(),
            });
        }
        extract_download_tar(&stdout, dest, base)?;
        Ok(())
    }

    /// Unused by the supervisor; logs are currently a human's tool.
    #[allow(dead_code)]
    pub async fn logs(&self, name: &str, tail: u32) -> Result<String> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = mock(&["logs".into(), name.into(), "-n".into(), tail.to_string()]);
            return Ok(out.stdout);
        }

        self.with_timeout("sandbox logs", self.default_timeout, || async {
            let mut client = self.connect().await?;
            let sb = get_sandbox(&mut client, name).await?;
            let resp = client
                .get_sandbox_logs(GetSandboxLogsRequest {
                    sandbox_id: sb.object_id().to_string(),
                    lines: tail,
                    since_ms: 0,
                    sources: vec![],
                    min_level: String::new(),
                    workspace: String::new(),
                })
                .await
                .map_err(|e| Error::Failed {
                    op: "sandbox logs".into(),
                    message: e.to_string(),
                })?;
            let lines = resp.into_inner().logs;
            Ok(lines
                .into_iter()
                .map(|l| l.message)
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .await
    }

    /// Run a command in a sandbox and wait for it.
    pub async fn exec(&self, name: &str, script: &str, timeout: Duration) -> Result<Output> {
        self.exec_with_stdin(name, script, Vec::new(), timeout).await
    }

    async fn exec_with_stdin(
        &self,
        name: &str,
        script: &str,
        stdin: Vec<u8>,
        timeout: Duration,
    ) -> Result<Output> {
        let (code, stdout, stderr) = self.exec_capture(name, script, stdin, timeout).await?;
        Ok(Output {
            code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Like [`Self::exec`], but keep stdout/stderr as raw bytes.
    ///
    /// Required for `download` (USTAR over the wire). Text paths may keep using
    /// [`Output`] — lossy UTF-8 is fine for logs and shell probes.
    async fn exec_capture(
        &self,
        name: &str,
        script: &str,
        stdin: Vec<u8>,
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let _ = &stdin;
            let remote = timeout.as_secs().saturating_sub(5).max(1);
            let args = [
                "sandbox".into(),
                "exec".into(),
                "-n".into(),
                name.into(),
                "--timeout".into(),
                remote.to_string(),
                "--".into(),
                "bash".into(),
                "-lc".into(),
                script.into(),
            ];
            let out = mock(&args);
            return Ok((out.code, out.stdout.into_bytes(), out.stderr.into_bytes()));
        }

        let remote = timeout.as_secs().saturating_sub(5).max(1);
        let started = Instant::now();
        let attempt = ExecAttemptCtx::new();
        let result = self
            .with_timeout(&format!("sandbox exec {name}"), timeout, || {
                let attempt = attempt.clone();
                let name = name.to_string();
                async move {
                    let mut client = self.connect().await?;
                    let sb = get_sandbox(&mut client, &name).await?;
                    let sandbox_id = sb.object_id().to_string();
                    attempt.set_sandbox_id(sandbox_id.clone());
                    // Client-generated id so journalctl can still correlate when the
                    // gateway drops the h2 stream before response headers arrive.
                    let client_request_id = new_exec_request_id();
                    attempt.set_request_id(client_request_id.clone());
                    let request = exec_sandbox_tonic_request(
                        ExecSandboxRequest {
                            sandbox_id,
                            command: vec!["bash".into(), "-lc".into(), script.to_string()],
                            workdir: String::new(),
                            environment: Default::default(),
                            timeout_seconds: u32::try_from(remote).unwrap_or(u32::MAX),
                            stdin,
                            tty: false,
                            cols: 0,
                            rows: 0,
                        },
                        &client_request_id,
                    );
                    let response = client.exec_sandbox(request).await.map_err(|status| {
                        attempt.observe_status_metadata(&status);
                        Error::Failed {
                            op: "sandbox exec".into(),
                            message: format_exec_status(&status),
                        }
                    })?;
                    attempt.observe_metadata(response.metadata());
                    let mut stream = response.into_inner();

                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    let mut code = -1;
                    while let Some(ev) = stream.next().await {
                        let ev = ev.map_err(|status| {
                            attempt.observe_status_metadata(&status);
                            Error::Failed {
                                op: "sandbox exec".into(),
                                message: format_exec_status(&status),
                            }
                        })?;
                        apply_exec_event(ev, &mut stdout, &mut stderr, &mut code);
                    }
                    Ok((code, stdout, stderr))
                }
            })
            .await;
        finish_exec_attempt(
            result,
            self.endpoint.as_deref(),
            name,
            &attempt,
            started,
        )
    }

    /// Run a command and hand every stdout line to `on_line` as it arrives.
    ///
    /// This is how liveness and cost stay *observed rather than self-reported*:
    /// the supervisor watches `claude --output-format stream-json` go by and
    /// heartbeats on real activity, so a hung agent cannot claim to be fine.
    ///
    /// `on_line` is called from the read loop, so it must not block.
    pub async fn exec_streaming<F>(
        &self,
        name: &str,
        script: &str,
        timeout: Duration,
        mut on_line: F,
    ) -> Result<Output>
    where
        F: FnMut(&str) + Send,
    {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let out = self.exec(name, script, timeout).await?;
            for line in out.stdout.lines() {
                on_line(line);
            }
            let _ = mock;
            return Ok(out);
        }

        let remote = timeout.as_secs().saturating_sub(5).max(1);
        let started = Instant::now();
        let attempt = ExecAttemptCtx::new();
        let result = self
            .with_timeout(&format!("sandbox exec {name}"), timeout, || {
                let attempt = attempt.clone();
                let name = name.to_string();
                async move {
                    let mut client = self.connect().await?;
                    let sb = get_sandbox(&mut client, &name).await?;
                    let sandbox_id = sb.object_id().to_string();
                    attempt.set_sandbox_id(sandbox_id.clone());
                    let client_request_id = new_exec_request_id();
                    attempt.set_request_id(client_request_id.clone());
                    let request = exec_sandbox_tonic_request(
                        ExecSandboxRequest {
                            sandbox_id,
                            command: vec!["bash".into(), "-lc".into(), script.to_string()],
                            workdir: String::new(),
                            environment: Default::default(),
                            timeout_seconds: u32::try_from(remote).unwrap_or(u32::MAX),
                            stdin: Vec::new(),
                            tty: false,
                            cols: 0,
                            rows: 0,
                        },
                        &client_request_id,
                    );
                    let response = client.exec_sandbox(request).await.map_err(|status| {
                        attempt.observe_status_metadata(&status);
                        Error::Failed {
                            op: "sandbox exec".into(),
                            message: format_exec_status(&status),
                        }
                    })?;
                    attempt.observe_metadata(response.metadata());
                    let mut stream = response.into_inner();

                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    let mut code = -1;
                    let mut line_buf = String::new();
                    while let Some(ev) = stream.next().await {
                        let ev = ev.map_err(|status| {
                            attempt.observe_status_metadata(&status);
                            Error::Failed {
                                op: "sandbox exec".into(),
                                message: format_exec_status(&status),
                            }
                        })?;
                        if let Some(exec_sandbox_event::Payload::Stdout(chunk)) = &ev.payload {
                            stdout.extend_from_slice(&chunk.data);
                            let text = String::from_utf8_lossy(&chunk.data);
                            line_buf.push_str(&text);
                            while let Some(pos) = line_buf.find('\n') {
                                let line = line_buf[..pos].to_string();
                                line_buf.drain(..=pos);
                                on_line(&line);
                            }
                        } else {
                            apply_exec_event(ev, &mut stdout, &mut stderr, &mut code);
                        }
                    }
                    if !line_buf.is_empty() {
                        on_line(&line_buf);
                    }
                    Ok(Output {
                        code,
                        stdout: String::from_utf8_lossy(&stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    })
                }
            })
            .await;
        finish_exec_attempt(
            result,
            self.endpoint.as_deref(),
            name,
            &attempt,
            started,
        )
    }

    /// Interactive TTY attach via `ExecSandboxInteractive` (no local OpenSSH).
    ///
    /// This is the in-process path Cockpit uses — same interactive shell shape
    /// as `sandbox connect`, but over gRPC so a browser WebSocket can relay it.
    pub async fn exec_interactive(
        &self,
        name: &str,
        command: Vec<String>,
        cols: u32,
        rows: u32,
    ) -> Result<InteractiveExec> {
        self.exec_interactive_inner(name, command, true, cols, rows)
            .await
    }

    /// Same gRPC path as [`exec_interactive`](Self::exec_interactive) but
    /// without a pty — no line/echo mangling, no window size. For driving a
    /// byte-oriented protocol over the pipe (cockpit MCP's `nc -lU` relay)
    /// rather than a terminal.
    pub async fn exec_interactive_raw(
        &self,
        name: &str,
        command: Vec<String>,
    ) -> Result<InteractiveExec> {
        self.exec_interactive_inner(name, command, false, 0, 0)
            .await
    }

    async fn exec_interactive_inner(
        &self,
        name: &str,
        command: Vec<String>,
        tty: bool,
        cols: u32,
        rows: u32,
    ) -> Result<InteractiveExec> {
        if command.is_empty() {
            return Err(Error::Failed {
                op: "exec interactive".into(),
                message: "command required".into(),
            });
        }

        #[cfg(test)]
        if let Some(mock) = &self.mock {
            let mut args = vec![
                "sandbox".into(),
                "exec-interactive".into(),
                "-n".into(),
                name.into(),
                "--".into(),
            ];
            args.extend(command.clone());
            let out = mock(&args);
            if !out.ok() {
                return Err(Error::Failed {
                    op: "exec interactive".into(),
                    message: out.stderr.trim().to_string(),
                });
            }
            let (input_tx, _input_rx) = mpsc::channel::<ExecSandboxInput>(4);
            let (event_tx, event_rx) = mpsc::channel(16);
            let stdout = out.stdout.into_bytes();
            let code = out.code;
            let join = tokio::spawn(async move {
                if !stdout.is_empty() {
                    let _ = event_tx.send(InteractiveEvent::Stdout(stdout)).await;
                }
                let _ = event_tx.send(InteractiveEvent::Exit(code)).await;
            });
            return Ok(InteractiveExec {
                input_tx,
                events: event_rx,
                join: Some(join),
            });
        }

        // Setup (resolve sandbox + open stream) is deadline-bound; the session
        // itself lives until Exit / drop — same hang-vs-deadline rule as exec.
        let started = Instant::now();
        let attempt = ExecAttemptCtx::new();
        let endpoint = self.endpoint.clone();
        let result = self
            .with_timeout(
                &format!("exec interactive {name}"),
                self.default_timeout,
                || {
                    let attempt = attempt.clone();
                    let name = name.to_string();
                    let endpoint = endpoint.clone();
                    async move {
                        let mut client = self.connect().await?;
                        let sb = get_sandbox(&mut client, &name).await?;
                        let sandbox_id = sb.object_id().to_string();
                        attempt.set_sandbox_id(sandbox_id.clone());
                        let client_request_id = new_exec_request_id();
                        attempt.set_request_id(client_request_id.clone());
                        let (input_tx, input_rx) = mpsc::channel::<ExecSandboxInput>(4096);
                        input_tx
                            .send(ExecSandboxInput {
                                payload: Some(exec_sandbox_input::Payload::Start(
                                    ExecSandboxRequest {
                                        sandbox_id,
                                        command,
                                        workdir: String::new(),
                                        environment: Default::default(),
                                        timeout_seconds: 0,
                                        stdin: Vec::new(),
                                        tty,
                                        cols,
                                        rows,
                                    },
                                )),
                            })
                            .await
                            .map_err(|_| Error::Failed {
                                op: "exec interactive".into(),
                                message: "failed to queue start frame".into(),
                            })?;

                        let mut req = tonic::Request::new(ReceiverStream::new(input_rx));
                        if let Ok(v) = client_request_id.parse() {
                            req.metadata_mut().insert("x-request-id", v);
                        }
                        let response = client
                            .exec_sandbox_interactive(req)
                            .await
                            .map_err(|status| {
                                attempt.observe_status_metadata(&status);
                                Error::Failed {
                                    op: "exec interactive".into(),
                                    message: format_exec_status(&status),
                                }
                            })?;
                        attempt.observe_metadata(response.metadata());
                        let mut stream = response.into_inner();

                        let (event_tx, event_rx) = mpsc::channel(256);
                        let log_sandbox_id = attempt.sandbox_id();
                        let log_request_id = attempt.request_id();
                        let join = tokio::spawn(async move {
                            while let Some(ev) = stream.next().await {
                                let ev = match ev {
                                    Ok(e) => e,
                                    Err(e) => {
                                        let status_rid = grpc_request_id(e.metadata());
                                        let request_id = status_rid
                                            .as_deref()
                                            .or(log_request_id.as_deref())
                                            .unwrap_or("");
                                        let msg = format_exec_status(&e);
                                        // Cockpit Stop / sandbox delete tears the relay down
                                        // while attach or MCP still has an interactive stream
                                        // open — expected, not a fault to page on.
                                        if is_expected_interactive_disconnect(&msg) {
                                            tracing::debug!(
                                                gateway_endpoint = endpoint.as_deref().unwrap_or(""),
                                                sandbox_name = %name,
                                                sandbox_id = log_sandbox_id.as_deref().unwrap_or(""),
                                                request_id,
                                                "exec interactive stream closed on teardown: {msg}"
                                            );
                                        } else {
                                            tracing::warn!(
                                                gateway_endpoint = endpoint.as_deref().unwrap_or(""),
                                                sandbox_name = %name,
                                                sandbox_id = log_sandbox_id.as_deref().unwrap_or(""),
                                                request_id,
                                                "exec interactive stream error: {msg}"
                                            );
                                        }
                                        break;
                                    }
                                };
                                let mapped = match ev.payload {
                                    Some(exec_sandbox_event::Payload::Stdout(chunk)) => {
                                        InteractiveEvent::Stdout(chunk.data)
                                    }
                                    Some(exec_sandbox_event::Payload::Stderr(chunk)) => {
                                        InteractiveEvent::Stderr(chunk.data)
                                    }
                                    Some(exec_sandbox_event::Payload::Exit(exit)) => {
                                        InteractiveEvent::Exit(exit.exit_code)
                                    }
                                    None => continue,
                                };
                                let is_exit = matches!(mapped, InteractiveEvent::Exit(_));
                                if event_tx.send(mapped).await.is_err() {
                                    break;
                                }
                                if is_exit {
                                    break;
                                }
                            }
                        });

                        Ok(InteractiveExec {
                            input_tx,
                            events: event_rx,
                            join: Some(join),
                        })
                    }
                },
            )
            .await;
        // Setup failures only — mid-stream errors log inside the join task.
        finish_exec_attempt(
            result,
            self.endpoint.as_deref(),
            name,
            &attempt,
            started,
        )
    }
}

/// Let's Encrypt Generation Y trust anchors (public roots). See
/// https://letsencrypt.org/ca/certificates/ — not yet in webpki-roots.
const LETS_ENCRYPT_GEN_Y_ROOTS: &str = concat!(
    include_str!("tls_roots/isrg-root-ye.pem"),
    "\n",
    include_str!("tls_roots/isrg-root-yr.pem"),
);

fn mtls_bundle_complete(b: &OpenShellMtlsBundle) -> bool {
    !b.ca_pem.trim().is_empty()
        && !b.client_cert_pem.trim().is_empty()
        && !b.client_key_pem.trim().is_empty()
}

/// Host for TLS SNI — tonic usually infers this, but edge proxies are happier
/// when it is explicit.
fn tls_domain_name(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))?;
    let authority = rest.split('/').next().unwrap_or("");
    // userinfo@host:port → host
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = hostport
        .strip_prefix('[')
        .and_then(|s| s.split(']').next())
        .unwrap_or_else(|| hostport.split(':').next().unwrap_or(""));
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// tonic's Display for transport failures is just "transport error"; the useful
/// bit (cert, DNS, reset) lives in `source()`.
fn format_transport_error(err: &tonic::transport::Error) -> String {
    let mut out = err.to_string();
    let mut src = std::error::Error::source(err);
    while let Some(e) = src {
        out.push_str(": ");
        out.push_str(&e.to_string());
        src = e.source();
    }
    if out.contains("UnknownIssuer") {
        out.push_str(
            " (is Gateway endpoint the OIDC HTTPS URL? a local mTLS gateway needs auth mode mTLS, or its CA pinned)",
        );
    }
    out
}

/// Per-attempt correlation for ExecSandbox / interactive setup failures.
///
/// Populated as the call progresses so a timeout or mid-stream h2 drop can still
/// log gateway endpoint + sandbox id + request id for journalctl greps. Never
/// holds secrets (no PEMs, tokens, scripts, or stdin).
#[derive(Clone, Default)]
struct ExecAttemptCtx {
    inner: Arc<Mutex<ExecAttemptInner>>,
}

#[derive(Default)]
struct ExecAttemptInner {
    sandbox_id: Option<String>,
    request_id: Option<String>,
}

impl ExecAttemptCtx {
    fn new() -> Self {
        Self::default()
    }

    fn set_sandbox_id(&self, id: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.sandbox_id = Some(id);
        }
    }

    fn set_request_id(&self, id: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.request_id = Some(id);
        }
    }

    fn sandbox_id(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|g| g.sandbox_id.clone())
    }

    fn request_id(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|g| g.request_id.clone())
    }

    fn observe_metadata(&self, md: &tonic::metadata::MetadataMap) {
        if let Some(id) = grpc_request_id(md) {
            self.set_request_id(id);
        }
    }

    fn observe_status_metadata(&self, status: &tonic::Status) {
        self.observe_metadata(status.metadata());
    }
}

/// OpenShell gateway echoes `x-request-id` (tower-http SetRequestId). Also accept
/// common trace headers when a proxy injects them instead.
fn grpc_request_id(md: &tonic::metadata::MetadataMap) -> Option<String> {
    const KEYS: &[&str] = &[
        "x-request-id",
        "x-correlation-id",
        "request-id",
        "traceparent",
    ];
    for key in KEYS {
        if let Some(v) = md.get(*key).and_then(|val| val.to_str().ok()) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn new_exec_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn exec_sandbox_tonic_request(
    body: ExecSandboxRequest,
    request_id: &str,
) -> tonic::Request<ExecSandboxRequest> {
    let mut req = tonic::Request::new(body);
    if let Ok(v) = request_id.parse() {
        req.metadata_mut().insert("x-request-id", v);
    }
    req
}

fn format_exec_status(status: &tonic::Status) -> String {
    // Prefer Display over Debug — avoids dumping the whole Status shape and
    // keeps Authorization material out of the message if a proxy stuffed it
    // into metadata we do not read.
    status.to_string()
}

fn log_sandbox_exec_failure(
    endpoint: Option<&str>,
    sandbox_name: &str,
    sandbox_id: Option<&str>,
    elapsed: Duration,
    request_id: Option<&str>,
    err: &Error,
) {
    tracing::warn!(
        gateway_endpoint = endpoint.unwrap_or(""),
        sandbox_name,
        sandbox_id = sandbox_id.unwrap_or(""),
        elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        request_id = request_id.unwrap_or(""),
        error = %err,
        "openshell exec failed"
    );
}

/// Enrich + log a failed exec attempt. Success is returned unchanged.
fn finish_exec_attempt<T>(
    result: Result<T>,
    endpoint: Option<&str>,
    sandbox_name: &str,
    attempt: &ExecAttemptCtx,
    started: Instant,
) -> Result<T> {
    match result {
        Ok(v) => Ok(v),
        Err(err) => {
            let sandbox_id = attempt.sandbox_id();
            let request_id = attempt.request_id();
            let elapsed = started.elapsed();
            let err = enrich_exec_error(
                err,
                endpoint,
                sandbox_name,
                sandbox_id.as_deref(),
                request_id.as_deref(),
                elapsed,
            );
            log_sandbox_exec_failure(
                endpoint,
                sandbox_name,
                sandbox_id.as_deref(),
                elapsed,
                request_id.as_deref(),
                &err,
            );
            Err(err)
        }
    }
}

/// Append correlatable context to the error string (still no secrets).
fn enrich_exec_error(
    err: Error,
    endpoint: Option<&str>,
    sandbox_name: &str,
    sandbox_id: Option<&str>,
    request_id: Option<&str>,
    elapsed: Duration,
) -> Error {
    let ctx = format!(
        "endpoint={} sandbox_name={} sandbox_id={} elapsed_ms={} request_id={}",
        endpoint.unwrap_or(""),
        sandbox_name,
        sandbox_id.unwrap_or(""),
        elapsed.as_millis(),
        request_id.unwrap_or(""),
    );
    match err {
        Error::Failed { op, message } => Error::Failed {
            op,
            message: if message.contains("endpoint=") {
                message
            } else {
                format!("{message} ({ctx})")
            },
        },
        Error::Timeout { op, secs } => Error::Timeout {
            op: if op.contains("endpoint=") {
                op
            } else {
                format!("{op} ({ctx})")
            },
            secs,
        },
        Error::Connect(message) => Error::Connect(if message.contains("endpoint=") {
            message
        } else {
            format!("{message} ({ctx})")
        }),
        other => other,
    }
}

fn apply_exec_event(
    ev: ExecSandboxEvent,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    code: &mut i32,
) {
    match ev.payload {
        Some(exec_sandbox_event::Payload::Stdout(chunk)) => stdout.extend_from_slice(&chunk.data),
        Some(exec_sandbox_event::Payload::Stderr(chunk)) => stderr.extend_from_slice(&chunk.data),
        Some(exec_sandbox_event::Payload::Exit(exit)) => *code = exit.exit_code,
        None => {}
    }
}

async fn get_sandbox(
    client: &mut OsClient,
    name: &str,
) -> Result<openshell_core::proto::Sandbox> {
    let resp = client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
            workspace: String::new(),
        })
        .await
        .map_err(|e| Error::Failed {
            op: "get sandbox".into(),
            message: e.to_string(),
        })?;
    resp.into_inner().sandbox.ok_or_else(|| Error::Failed {
        op: "get sandbox".into(),
        message: format!("sandbox `{name}` missing from response"),
    })
}

/// Human/CLI phase names for [`Sandbox::phase`]. Must stay aligned with
/// `wait_until_sandbox_ready` (and OpenShell `sandbox list -o json`).
fn phase_label(phase: i32) -> String {
    match SandboxPhase::try_from(phase).unwrap_or(SandboxPhase::Unspecified) {
        SandboxPhase::Ready => "Ready".into(),
        SandboxPhase::Provisioning => "Provisioning".into(),
        SandboxPhase::Error => "Error".into(),
        SandboxPhase::Deleting => "Deleting".into(),
        SandboxPhase::Unknown => "Unknown".into(),
        SandboxPhase::Unspecified => "Unspecified".into(),
    }
}

async fn wait_ready(
    client: &mut OsClient,
    name: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(250);
    loop {
        let sb = get_sandbox(client, name).await?;
        let phase = SandboxPhase::try_from(sb.phase()).unwrap_or(SandboxPhase::Unspecified);
        match phase {
            SandboxPhase::Ready => return Ok(()),
            SandboxPhase::Error => {
                return Err(Error::Failed {
                    op: "sandbox create".into(),
                    message: format!("sandbox `{name}` entered error phase"),
                });
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout {
                op: format!("wait ready {name}"),
                secs: timeout.as_secs(),
            });
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

fn build_create_request(spec: &SandboxSpec) -> Result<CreateSandboxRequest> {
    let policy = match &spec.policy {
        Some(yaml) if !yaml.trim().is_empty() => Some(
            openshell_policy::parse_sandbox_policy(yaml)
                .map_err(|e| Error::Policy(e.to_string()))?,
        ),
        _ => None,
    };
    let resources = resource_limits(spec.cpu.as_deref(), spec.memory.as_deref())?;
    let template = Some(SandboxTemplate {
        image: spec.from.clone(),
        resources,
        ..SandboxTemplate::default()
    });
    let environment: BTreeMap<String, String> = spec.env.iter().cloned().collect();
    let labels: BTreeMap<String, String> = spec.labels.iter().cloned().collect();
    Ok(CreateSandboxRequest {
        spec: Some(ProtoSandboxSpec {
            environment: environment.into_iter().collect(),
            policy,
            providers: spec.providers.clone(),
            template,
            ..ProtoSandboxSpec::default()
        }),
        name: spec.name.clone(),
        labels: labels.into_iter().collect(),
        annotations: Default::default(),
        workspace: String::new(),
    })
}

fn resource_limits(cpu: Option<&str>, memory: Option<&str>) -> Result<Option<Struct>> {
    let mut limits = BTreeMap::new();
    if let Some(cpu) = cpu.map(str::trim).filter(|s| !s.is_empty()) {
        limits.insert(
            "cpu".into(),
            Value {
                kind: Some(Kind::StringValue(cpu.to_string())),
            },
        );
    }
    if let Some(memory) = memory.map(str::trim).filter(|s| !s.is_empty()) {
        limits.insert(
            "memory".into(),
            Value {
                kind: Some(Kind::StringValue(memory.to_string())),
            },
        );
    }
    if limits.is_empty() {
        return Ok(None);
    }
    let mut fields = BTreeMap::new();
    fields.insert(
        "limits".into(),
        Value {
            kind: Some(Kind::StructValue(Struct { fields: limits })),
        },
    );
    Ok(Some(Struct { fields }))
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn gateway_provider_from_proto(p: Provider) -> GatewayProvider {
    let name = p
        .metadata
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or_default();
    let mut credential_keys: Vec<_> = p.credentials.keys().cloned().collect();
    credential_keys.sort();
    let mut config_keys: Vec<_> = p.config.keys().cloned().collect();
    config_keys.sort();
    GatewayProvider {
        name,
        provider_type: p.r#type,
        credential_keys,
        config_keys,
    }
}

fn provider_type_profile_from_proto(p: ProtoProviderProfile) -> ProviderTypeProfile {
    let mut credential_env_vars = Vec::new();
    for cred in &p.credentials {
        for env in &cred.env_vars {
            if !env.trim().is_empty() {
                credential_env_vars.push(env.clone());
            }
        }
    }
    credential_env_vars.sort();
    credential_env_vars.dedup();
    // Profiles rarely declare config keys in proto; leave empty for freeform UI.
    ProviderTypeProfile {
        id: p.id,
        display_name: p.display_name,
        description: p.description,
        category: format!("{:?}", p.category),
        credential_env_vars,
        config_keys: Vec::new(),
    }
}

fn refresh_strategy_from_name(name: &str) -> Result<ProviderCredentialRefreshStrategy> {
    let n = name.trim().to_ascii_lowercase().replace('-', "_");
    Ok(match n.as_str() {
        "oauth2_refresh_token" | "oauth2refreshtoken" => {
            ProviderCredentialRefreshStrategy::Oauth2RefreshToken
        }
        "google_service_account_jwt" | "googleserviceaccountjwt" => {
            ProviderCredentialRefreshStrategy::GoogleServiceAccountJwt
        }
        "static" => ProviderCredentialRefreshStrategy::Static,
        "external" => ProviderCredentialRefreshStrategy::External,
        "oauth2_client_credentials" => {
            ProviderCredentialRefreshStrategy::Oauth2ClientCredentials
        }
        "aws_sts_assume_role" => ProviderCredentialRefreshStrategy::AwsStsAssumeRole,
        other => {
            return Err(Error::Failed {
                op: "provider refresh".into(),
                message: format!("unknown refresh strategy {other:?}"),
            });
        }
    })
}

fn split_sandbox_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (".", path),
    }
}

fn upload_dest_parts(local: &Path, dest: &str) -> Result<(String, String)> {
    // Dest is always a directory (OpenShell CLI / docs). Treating paths like
    // `/sandbox/.sandboard` as a *file* named `.sandboard` wrote report.schema.json on
    // top of the verdict dir and broke escalate/report.
    let tar_name = local
        .file_name()
        .ok_or_else(|| Error::Failed {
            op: "sandbox upload".into(),
            message: format!("path has no file name: {}", local.display()),
        })?
        .to_string_lossy()
        .into_owned();
    let dest_dir = dest.trim_end_matches('/').to_string();
    if dest_dir.is_empty() {
        return Err(Error::Failed {
            op: "sandbox upload".into(),
            message: "destination directory is empty".into(),
        });
    }
    Ok((dest_dir, tar_name))
}

fn build_upload_tar(local: &Path, tar_name: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut ar = tar::Builder::new(&mut buf);
        if local.is_dir() {
            ar.append_dir_all(tar_name, local).map_err(Error::Io)?;
        } else {
            let mut file = std::fs::File::open(local).map_err(Error::Io)?;
            ar.append_file(tar_name, &mut file).map_err(Error::Io)?;
        }
        ar.finish().map_err(Error::Io)?;
    }
    Ok(buf)
}

fn extract_download_tar(bytes: &[u8], dest: &str, expected_base: &str) -> Result<()> {
    let dest_path = PathBuf::from(dest);
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let mut ar = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut wrote = false;
    for entry in ar.entries().map_err(Error::Io)? {
        let mut entry = entry.map_err(Error::Io)?;
        let path = entry.path().map_err(Error::Io)?.into_owned();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name != expected_base && path.to_string_lossy() != expected_base {
            continue;
        }
        if dest_path
            .parent()
            .map(|p| p.exists() && p.is_dir() && dest_path.extension().is_none() && !dest.ends_with('/'))
            .unwrap_or(false)
            && dest_path.is_dir()
        {
            let target = dest_path.join(&name);
            let mut out = std::fs::File::create(&target).map_err(Error::Io)?;
            std::io::copy(&mut entry, &mut out).map_err(Error::Io)?;
        } else {
            let mut out = std::fs::File::create(&dest_path).map_err(Error::Io)?;
            std::io::copy(&mut entry, &mut out).map_err(Error::Io)?;
            out.flush().map_err(Error::Io)?;
        }
        wrote = true;
        break;
    }
    if !wrote {
        // Fallback: treat stdout as raw file bytes (cat semantics).
        std::fs::write(&dest_path, bytes).map_err(Error::Io)?;
    }
    Ok(())
}

/// Argv-shaped create call for unit-test mocks (image flag remains `--from`).
#[cfg(test)]
fn mock_create_args(spec: &SandboxSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "sandbox".into(),
        "create".into(),
        "--name".into(),
        spec.name.clone(),
        "--from".into(),
        spec.from.clone(),
        "--no-tty".into(),
    ];
    for p in &spec.providers {
        args.push("--provider".into());
        args.push(p.clone());
    }
    if let Some(policy) = &spec.policy {
        args.push("--policy".into());
        args.push(policy.clone());
    }
    for (k, v) in &spec.env {
        args.push("--env".into());
        args.push(format!("{k}={v}"));
    }
    for (k, v) in &spec.labels {
        args.push("--label".into());
        args.push(format!("{k}={v}"));
    }
    if let Some(cpu) = &spec.cpu {
        args.push("--cpu".into());
        args.push(cpu.clone());
    }
    if let Some(mem) = &spec.memory {
        args.push("--memory".into());
        args.push(mem.clone());
    }
    args
}

/// Interactive exec ended because the sandbox/relay went away mid-stream.
///
/// Happens on every cockpit Stop (and card halt) while attach or the MCP
/// stdio relay still holds `ExecSandboxInteractive`. Not a fault — the delete
/// won the race with the stream.
pub(crate) fn is_expected_interactive_disconnect(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("exec relay closed")
        || e.contains("the service is currently unavailable")
        || e.contains("sandbox not found")
        || e.contains("entity was not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_interactive_disconnect_matches_teardown_relay() {
        assert!(is_expected_interactive_disconnect(
            "status: Some(Status { code: Unavailable, message: \"exec relay closed before the command reported an exit status\", .. })"
        ));
        assert!(is_expected_interactive_disconnect(
            "code: 'The service is currently unavailable', message: \"exec relay closed\""
        ));
        assert!(is_expected_interactive_disconnect("sandbox not found"));
        assert!(!is_expected_interactive_disconnect("permission denied writing tty"));
    }

    #[test]
    fn grpc_request_id_prefers_x_request_id() {
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert("traceparent", "00-abc-def-01".parse().unwrap());
        md.insert("x-request-id", "gw-corr-1".parse().unwrap());
        assert_eq!(grpc_request_id(&md).as_deref(), Some("gw-corr-1"));
    }

    #[test]
    fn grpc_request_id_falls_back_to_traceparent() {
        let mut md = tonic::metadata::MetadataMap::new();
        md.insert("traceparent", "00-abc-def-01".parse().unwrap());
        assert_eq!(grpc_request_id(&md).as_deref(), Some("00-abc-def-01"));
        assert!(grpc_request_id(&tonic::metadata::MetadataMap::new()).is_none());
    }

    #[test]
    fn exec_sandbox_request_injects_x_request_id() {
        let req = exec_sandbox_tonic_request(
            ExecSandboxRequest {
                sandbox_id: "sb-1".into(),
                command: vec!["true".into()],
                workdir: String::new(),
                environment: Default::default(),
                timeout_seconds: 1,
                stdin: Vec::new(),
                tty: false,
                cols: 0,
                rows: 0,
            },
            "client-corr-9",
        );
        assert_eq!(
            req.metadata()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("client-corr-9")
        );
    }

    #[test]
    fn enrich_exec_error_adds_correlation_context_without_secrets() {
        let err = enrich_exec_error(
            Error::Failed {
                op: "sandbox exec".into(),
                message: "transport error: h2 reset".into(),
            },
            Some("https://gateway.example.com"),
            "sandboard-card-59-a1",
            Some("sb-object-1"),
            Some("req-abc"),
            Duration::from_millis(1234),
        );
        let msg = err.to_string();
        assert!(msg.contains("https://gateway.example.com"), "{msg}");
        assert!(msg.contains("sandboard-card-59-a1"), "{msg}");
        assert!(msg.contains("sb-object-1"), "{msg}");
        assert!(msg.contains("elapsed_ms=1234"), "{msg}");
        assert!(msg.contains("request_id=req-abc"), "{msg}");
        assert!(!msg.to_ascii_lowercase().contains("bearer"));
        assert!(!msg.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn new_exec_request_id_is_hex_and_nonempty() {
        let a = new_exec_request_id();
        let b = new_exec_request_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            name: "sandboard-card-7".into(),
            from: "sandboard-sandbox:latest".into(),
            providers: vec!["vertex".into(), "gh-clankr".into()],
            policy: Some("version: 1\nfilesystem_policy:\n  include_workdir: true\n".into()),
            env: vec![("DISABLE_TELEMETRY".into(), "1".into())],
            labels: vec![(LABEL_ITEM.into(), "7".into())],
            cpu: Some("2".into()),
            memory: Some("4Gi".into()),
        }
    }

    #[tokio::test]
    async fn gateway_status_healthy_when_mock_status_ok() {
        let os = OpenShell::mock(
            |args| {
                assert_eq!(args, &["status".to_string()]);
                Output {
                    code: 0,
                    stdout: "Connected\nAuthenticated (mTLS transport)\n".into(),
                    stderr: String::new(),
                }
            },
            Duration::from_secs(5),
        );
        let st = os.gateway_status().await;
        assert!(st.healthy);
        assert!(!st.not_configured);
        assert!(st.summary.contains("Connected"));
        assert!(os.healthy().await);
    }

    #[tokio::test]
    async fn gateway_status_unhealthy_when_mock_fails() {
        let os = OpenShell::mock(
            |_| Output {
                code: 1,
                stdout: String::new(),
                stderr: "gateway unreachable".into(),
            },
            Duration::from_secs(5),
        );
        let st = os.gateway_status().await;
        assert!(!st.healthy);
        assert!(!st.not_configured);
        assert!(st.summary.contains("gateway unreachable"));
        assert!(!os.healthy().await);
    }

    #[tokio::test]
    async fn gateway_status_not_configured_without_endpoint() {
        let os = OpenShell::new(None, None, Duration::from_secs(5));
        let st = os.gateway_status().await;
        assert!(!st.healthy);
        assert!(st.not_configured, "summary={}", st.summary);
        assert!(st.summary.contains("endpoint") || st.summary.contains("not configured"));
        assert!(!os.healthy().await);
    }

    #[tokio::test]
    async fn gateway_status_rejects_http_plaintext() {
        let os = OpenShell::new(
            Some("http://gateway.example.com".into()),
            Some(GatewayAuth::OidcIncomplete {
                config: crate::model::OpenShellOidcConfig {
                    issuer: "https://idp.example.com".into(),
                    client_id: "openshell-cli".into(),
                    audience: "openshell-cli".into(),
                },
            }),
            Duration::from_secs(5),
        );
        let st = os.gateway_status().await;
        assert!(!st.healthy);
        assert!(st.not_configured, "summary={}", st.summary);
        assert!(
            st.summary.contains("https://"),
            "summary={}",
            st.summary
        );
    }

    #[tokio::test]
    async fn gateway_status_requires_explicit_auth_mode() {
        let os = OpenShell::new(
            Some("https://gateway.example.com".into()),
            None,
            Duration::from_secs(5),
        );
        let st = os.gateway_status().await;
        assert!(!st.healthy);
        assert!(st.not_configured, "summary={}", st.summary);
        assert!(
            st.summary.contains("auth mode") || st.summary.contains("mTLS"),
            "summary={}",
            st.summary
        );
    }

    /// The image flag is `--from` in the mock argv surface (and in CreateSandbox
    /// template.image for the real client). Getting this wrong used to yield a
    /// confusing registry lookup.
    #[test]
    fn image_is_passed_as_from() {
        let args = mock_create_args(&spec());
        assert!(args.windows(2).any(|w| w[0] == "--from" && w[1] == "sandboard-sandbox:latest"));
        assert!(!args.iter().any(|a| a == "--image"));
    }

    #[test]
    fn create_request_sets_image_providers_labels_and_policy() {
        let req = build_create_request(&spec()).expect("policy parses");
        assert_eq!(req.name, "sandboard-card-7");
        assert_eq!(req.labels.get("sandboard.item").map(String::as_str), Some("7"));
        let sandbox_spec = req.spec.expect("spec");
        assert_eq!(sandbox_spec.providers, vec!["vertex", "gh-clankr"]);
        assert_eq!(
            sandbox_spec.template.as_ref().map(|t| t.image.as_str()),
            Some("sandboard-sandbox:latest")
        );
        assert!(sandbox_spec.policy.is_some());
        assert_eq!(
            sandbox_spec.environment.get("DISABLE_TELEMETRY").map(String::as_str),
            Some("1")
        );
    }

    #[tokio::test]
    async fn create_passes_inline_policy_yaml_to_mock() {
        let yaml = "version: 1\nfilesystem_policy:\n  include_workdir: true\n";
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(None::<String>));
        let seen_c = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                let i = args.iter().position(|a| a == "--policy").expect("--policy");
                *seen_c.lock() = Some(args[i + 1].clone());
                Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            Duration::from_secs(5),
        );
        let mut s = spec();
        s.policy = Some(yaml.into());
        os.create(&s).await.expect("create");
        assert_eq!(seen.lock().as_deref(), Some(yaml));
    }

    #[tokio::test]
    async fn unconfigured_client_is_not_healthy() {
        assert!(!OpenShell::default().healthy().await);
    }

    #[test]
    fn phase_label_matches_cli_json_not_prost_debug() {
        // Ready is protobuf value 2 — Debug would be "2", which broke readiness polls.
        assert_eq!(phase_label(SandboxPhase::Ready as i32), "Ready");
        assert_eq!(phase_label(SandboxPhase::Error as i32), "Error");
        assert_eq!(phase_label(SandboxPhase::Deleting as i32), "Deleting");
        assert_eq!(phase_label(99), "Unspecified");
    }

    #[test]
    fn upload_dest_is_always_a_directory() {
        // Regression: `/sandbox/.sandboard` used to be treated as a file named `.sandboard`.
        let (dir, name) = upload_dest_parts(
            Path::new("docs/schemas/report.schema.json"),
            "/sandbox/.sandboard",
        )
        .expect("parts");
        assert_eq!(dir, "/sandbox/.sandboard");
        assert_eq!(name, "report.schema.json");
        let (dir2, name2) =
            upload_dest_parts(Path::new("sandbox/Containerfile"), "/tmp").expect("containerfile");
        assert_eq!(dir2, "/tmp");
        assert_eq!(name2, "Containerfile");
    }

    #[tokio::test]
    async fn exec_interactive_mock_streams_stdout_then_exit() {
        let os = OpenShell::mock(
            |args| {
                assert_eq!(args[0], "sandbox");
                assert_eq!(args[1], "exec-interactive");
                assert!(args.windows(2).any(|w| w[0] == "-n" && w[1] == "sandboard-cockpit"));
                Output {
                    code: 0,
                    stdout: "ready\n".into(),
                    stderr: String::new(),
                }
            },
            Duration::from_secs(5),
        );
        let mut session = os
            .exec_interactive(
                "sandboard-cockpit",
                vec!["bash".into(), "-l".into()],
                80,
                24,
            )
            .await
            .expect("interactive");
        match session.next_event().await {
            Some(InteractiveEvent::Stdout(b)) => assert_eq!(b, b"ready\n"),
            other => panic!("expected stdout, got {other:?}"),
        }
        match session.next_event().await {
            Some(InteractiveEvent::Exit(0)) => {}
            other => panic!("expected exit 0, got {other:?}"),
        }
    }

    // ---- gateway-backed. `cargo test -- --ignored` against a real gateway.
    //
    // PEM paths come from the environment rather than a guessed location under
    // $HOME: sandboard does not read host config, and neither should its tests.
    #[tokio::test]
    #[ignore = "needs a running OpenShell gateway and SANDBOARD_TEST_MTLS_* pointing at PEMs"]
    async fn real_gateway_health_and_list() {
        let endpoint = std::env::var("SANDBOARD_OPENSHELL_ENDPOINT")
            .unwrap_or_else(|_| "https://127.0.0.1:17670".into());
        let read = |var: &str| {
            let path = std::env::var(var).unwrap_or_else(|_| panic!("set {var} to a PEM path"));
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
        };
        let bundle = crate::secrets::OpenShellMtlsBundle {
            ca_pem: read("SANDBOARD_TEST_MTLS_CA"),
            client_cert_pem: read("SANDBOARD_TEST_MTLS_CERT"),
            client_key_pem: read("SANDBOARD_TEST_MTLS_KEY"),
        };
        let os = OpenShell::new(
            Some(endpoint),
            Some(GatewayAuth::Mtls(bundle)),
            Duration::from_secs(30),
        );
        assert!(os.healthy().await);
        os.list().await.expect("sandbox list");
    }

    /// Live OIDC probe. Tokens come from env JSON (never from `~/.config/openshell`
    /// inside sandboard itself — paste after `openshell gateway login` if needed):
    /// `SANDBOARD_TEST_OIDC_TOKEN_JSON={"access_token":…,"refresh_token":…,"expires_at":…,"issuer":…,"client_id":…}`
    #[tokio::test]
    #[ignore = "needs HTTPS OIDC gateway + SANDBOARD_TEST_OIDC_TOKEN_JSON"]
    async fn real_gateway_oidc_health_and_list() {
        let endpoint = std::env::var("SANDBOARD_OPENSHELL_ENDPOINT")
            .expect("set SANDBOARD_OPENSHELL_ENDPOINT to the gateway base URL");
        let issuer = std::env::var("SANDBOARD_TEST_OIDC_ISSUER").expect(
            "set SANDBOARD_TEST_OIDC_ISSUER (e.g. https://<keycloak>/realms/openshell)",
        );
        let client_id =
            std::env::var("SANDBOARD_TEST_OIDC_CLIENT_ID").unwrap_or_else(|_| "openshell-cli".into());
        let audience =
            std::env::var("SANDBOARD_TEST_OIDC_AUDIENCE").unwrap_or_else(|_| client_id.clone());
        let raw = std::env::var("SANDBOARD_TEST_OIDC_TOKEN_JSON")
            .expect("set SANDBOARD_TEST_OIDC_TOKEN_JSON to an OIDC token bundle JSON object");
        let bundle: crate::secrets::OpenShellOidcBundle =
            serde_json::from_str(&raw).expect("parse SANDBOARD_TEST_OIDC_TOKEN_JSON");
        let os = OpenShell::new(
            Some(endpoint),
            Some(GatewayAuth::oidc(
                crate::model::OpenShellOidcConfig {
                    issuer,
                    client_id,
                    audience,
                },
                bundle,
                None,
                None,
            )),
            Duration::from_secs(30),
        );
        let st = os.gateway_status().await;
        assert!(st.healthy, "status={st:?}");
        os.list().await.expect("sandbox list");
    }
}

#[cfg(test)]
mod download_tar_tests {
    use super::*;

    fn sample_ustar(contents: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut ar = tar::Builder::new(&mut buf);
            let mut header = tar::Header::new_gnu();
            header.set_path("plan.json").unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append(&header, contents).unwrap();
            ar.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extract_download_tar_round_trips_ustar_bytes() {
        let payload = b"{\"summary\":\"ok\",\"tasks\":[]}\n";
        let tar = sample_ustar(payload);
        let dir = std::env::temp_dir().join(format!(
            "sandboard-download-tar-ok-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.json");
        extract_download_tar(&tar, dest.to_str().unwrap(), "plan.json").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Guards the download path: GNU `tar cf` writes binary uid/mtime (high
    /// bit set). `String::from_utf8_lossy` replaces those bytes and the archive
    /// checksum fails — the bug that sent Initial plan cards back to Backlog.
    #[test]
    fn utf8_lossy_corrupts_gnu_tar_binary_header_fields() {
        let mut tar = sample_ustar(b"{\"x\":1}");
        // Mimic GNU tar binary header fields (see ustar mode with high bit).
        assert!(tar.len() > 120);
        tar[108] = 0x80;
        tar[109] = 0x9e;
        let lossy = String::from_utf8_lossy(&tar).into_owned().into_bytes();
        assert_ne!(tar, lossy, "lossy UTF-8 must change the byte stream");
        let dir = std::env::temp_dir().join(format!(
            "sandboard-download-tar-bad-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.json");
        let err = extract_download_tar(&lossy, dest.to_str().unwrap(), "plan.json")
            .expect_err("corrupted ustar must fail");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("checksum")
                || msg.contains("Io")
                || msg.to_lowercase().contains("tar"),
            "expected checksum/io failure, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod cockpit_policy_create_tests {
    use super::*;

    #[test]
    fn build_create_request_accepts_minimal_policy() {
        let yaml = crate::seed_policies::MINIMAL_SANDBOX_POLICY.to_string();
        let spec = SandboxSpec {
            name: "sandboard-policy-test".into(),
            from: "sandboard-sandbox:latest".into(),
            providers: vec![],
            policy: Some(yaml),
            env: vec![],
            labels: vec![("sandboard.cockpit".into(), "1".into())],
            cpu: Some("1".into()),
            memory: Some("2Gi".into()),
        };
        let req = build_create_request(&spec).expect("parse+build");
        let policy = req
            .spec
            .as_ref()
            .expect("spec")
            .policy
            .as_ref()
            .expect("policy must be present on create request");
        let yaml_out =
            openshell_policy::serialize_sandbox_policy(policy).expect("serialize");
        assert!(
            yaml_out.contains("include_workdir") && yaml_out.contains("/sandbox"),
            "create request must keep minimal filesystem policy:\n{yaml_out}"
        );
    }
}
