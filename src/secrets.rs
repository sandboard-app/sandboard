//! At-rest encryption for operator secrets (OpenShell mTLS PEMs, GitHub App
//! private key, etc.).
//!
//! Ciphertext lives in the board database. The only host file is a 32-byte
//! master key at `~/.config/sandboard/master.key` (mode 0600), auto-created on first
//! use. Override with `SANDBOARD_MASTER_KEY_PATH` or `SANDBOARD_MASTER_KEY` (64 hex chars)
//! for tests / alternate installs.
//!
//! GET APIs must never return decrypted PEMs / webhook secrets / client secrets
//! — only presence flags (plus non-secret identifiers like App ID).

use chacha20poly1305::{
    aead::{Aead, Key, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
/// File magic so we can change the AEAD later without guessing.
const BLOB_PREFIX: &[u8] = b"sandboard1";

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("master key: {0}")]
    MasterKey(String),
    #[error("encrypt: {0}")]
    Encrypt(String),
    #[error("decrypt: {0}")]
    Decrypt(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// OpenShell gateway client mTLS material (plaintext, in memory only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellMtlsBundle {
    pub ca_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
}

impl OpenShellMtlsBundle {
    /// Soft check that pasted text looks like PEM (not a path / garbage).
    pub fn validate_pem_shape(&self) -> Result<(), SecretsError> {
        for (label, pem) in [
            ("ca", &self.ca_pem),
            ("client_cert", &self.client_cert_pem),
            ("client_key", &self.client_key_pem),
        ] {
            let t = pem.trim();
            if t.is_empty() {
                return Err(SecretsError::Encrypt(format!("{label}: empty PEM")));
            }
            if !t.contains("BEGIN") || !t.contains("END") {
                return Err(SecretsError::Encrypt(format!(
                    "{label}: expected PEM block (BEGIN … END)"
                )));
            }
        }
        Ok(())
    }
}

/// Presence flags safe to return over the API.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellMtlsStatus {
    pub ca: bool,
    pub client_cert: bool,
    pub client_key: bool,
    pub complete: bool,
}

/// OpenShell gateway OIDC token bundle (plaintext, in memory only).
///
/// Shape matches OpenShell CLI `oidc_token.json` so refresh semantics stay aligned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellOidcBundle {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; refresh when within ~30s of this instant.
    pub expires_at: u64,
    pub issuer: String,
    pub client_id: String,
}

impl OpenShellOidcBundle {
    pub fn validate_for_seal(&self) -> Result<(), SecretsError> {
        if self.access_token.trim().is_empty() {
            return Err(SecretsError::Encrypt("access_token: empty".into()));
        }
        if self.refresh_token.trim().is_empty() {
            return Err(SecretsError::Encrypt("refresh_token: empty".into()));
        }
        if self.issuer.trim().is_empty() {
            return Err(SecretsError::Encrypt("issuer: empty".into()));
        }
        if self.client_id.trim().is_empty() {
            return Err(SecretsError::Encrypt("client_id: empty".into()));
        }
        Ok(())
    }

    pub fn access_expiring_soon(&self, skew_secs: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_add(skew_secs) >= self.expires_at
    }
}

/// Presence flags for sealed OIDC material (never returns tokens).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellOidcStatus {
    pub logged_in: bool,
}

impl From<&OpenShellMtlsBundle> for OpenShellMtlsStatus {
    fn from(b: &OpenShellMtlsBundle) -> Self {
        let ca = !b.ca_pem.trim().is_empty();
        let client_cert = !b.client_cert_pem.trim().is_empty();
        let client_key = !b.client_key_pem.trim().is_empty();
        Self {
            ca,
            client_cert,
            client_key,
            complete: ca && client_cert && client_key,
        }
    }
}

/// GitHub App credentials for installation-token minting (plaintext, in memory only).
///
/// App ID / Client ID are not secret, but live in the same sealed blob so one
/// meta row is the source of truth. GET APIs may echo those identifiers after
/// decrypt; never echo the private key, webhook secret, or client secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubAppBundle {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub private_key_pem: String,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
}

impl GitHubAppBundle {
    /// Soft check: App ID + private key PEM shape. Optional fields may be empty.
    pub fn validate_for_seal(&self) -> Result<(), SecretsError> {
        if self.app_id.trim().is_empty() {
            return Err(SecretsError::Encrypt("app_id: empty".into()));
        }
        let pem = self.private_key_pem.trim();
        if pem.is_empty() {
            return Err(SecretsError::Encrypt("private_key: empty".into()));
        }
        if !pem.contains("BEGIN") || !pem.contains("END") || !pem.contains("PRIVATE KEY") {
            return Err(SecretsError::Encrypt(
                "private_key: expected PEM block (BEGIN … PRIVATE KEY … END)".into(),
            ));
        }
        Ok(())
    }
}

/// Presence flags safe to return over the API (plus complete = app_id + key).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubAppStatus {
    pub app_id: bool,
    pub private_key: bool,
    pub webhook_secret: bool,
    pub client_id: bool,
    pub client_secret: bool,
    pub complete: bool,
}

impl From<&GitHubAppBundle> for GitHubAppStatus {
    fn from(b: &GitHubAppBundle) -> Self {
        let app_id = !b.app_id.trim().is_empty();
        let private_key = !b.private_key_pem.trim().is_empty();
        let webhook_secret = !b.webhook_secret.trim().is_empty();
        let client_id = !b.client_id.trim().is_empty();
        let client_secret = !b.client_secret.trim().is_empty();
        Self {
            app_id,
            private_key,
            webhook_secret,
            client_id,
            client_secret,
            complete: app_id && private_key,
        }
    }
}

/// Local admin credentials + session signing key (plaintext, in memory only).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthBundle {
    #[serde(default)]
    pub admin_username: String,
    /// PHC string from Argon2id.
    #[serde(default)]
    pub password_hash: String,
    /// 32 random bytes, base64 — HMAC key for session cookies.
    #[serde(default)]
    pub session_key_b64: String,
}

impl AuthBundle {
    pub fn is_configured(&self) -> bool {
        !self.admin_username.trim().is_empty() && !self.password_hash.trim().is_empty()
    }

    pub fn session_key_bytes(&self) -> Result<Vec<u8>, SecretsError> {
        let raw = self.session_key_b64.trim();
        if raw.is_empty() {
            return Err(SecretsError::Decrypt("session_key: empty".into()));
        }
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, raw)
            .map_err(|e| SecretsError::Decrypt(format!("session_key base64: {e}")))
    }
}

/// Resolve the master-key path (tests: `SANDBOARD_MASTER_KEY_PATH`).
pub fn master_key_path() -> PathBuf {
    if let Ok(p) = std::env::var("SANDBOARD_MASTER_KEY_PATH") {
        let t = p.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sandboard")
        .join("master.key")
}

fn load_or_create_master_key(path: &Path) -> Result<[u8; KEY_LEN], SecretsError> {
    if let Ok(hex) = std::env::var("SANDBOARD_MASTER_KEY") {
        let hex = hex.trim();
        if !hex.is_empty() {
            return parse_hex_key(hex);
        }
    }
    if path.exists() {
        let raw = fs::read(path)?;
        if raw.len() != KEY_LEN {
            return Err(SecretsError::MasterKey(format!(
                "{}: expected {KEY_LEN} bytes, got {}",
                path.display(),
                raw.len()
            )));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&raw);
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let mut key = [0u8; KEY_LEN];
    rand::rng().fill(&mut key);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(&key)?;
    f.sync_all()?;
    Ok(key)
}

fn parse_hex_key(hex: &str) -> Result<[u8; KEY_LEN], SecretsError> {
    if hex.len() != KEY_LEN * 2 {
        return Err(SecretsError::MasterKey(format!(
            "SANDBOARD_MASTER_KEY: expected {} hex chars, got {}",
            KEY_LEN * 2,
            hex.len()
        )));
    }
    let mut key = [0u8; KEY_LEN];
    for i in 0..KEY_LEN {
        key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| {
            SecretsError::MasterKey(format!("SANDBOARD_MASTER_KEY: invalid hex: {e}"))
        })?;
    }
    Ok(key)
}

fn cipher(key_bytes: &[u8; KEY_LEN]) -> ChaCha20Poly1305 {
    let key = Key::<ChaCha20Poly1305>::from(*key_bytes);
    ChaCha20Poly1305::new(&key)
}

/// Seal a UTF-8 JSON (or any bytes) payload → `sandboard1` || nonce || ciphertext, base64.
pub fn seal(plaintext: &[u8]) -> Result<String, SecretsError> {
    let key = load_or_create_master_key(&master_key_path())?;
    let cipher = cipher(&key);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|e| SecretsError::Encrypt(format!("nonce: {e}")))?;
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| SecretsError::Encrypt(e.to_string()))?;
    let mut out = Vec::with_capacity(BLOB_PREFIX.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(BLOB_PREFIX);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &out,
    ))
}

/// Open a blob produced by [`seal`].
pub fn open(sealed_b64: &str) -> Result<Vec<u8>, SecretsError> {
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        sealed_b64.trim(),
    )
    .map_err(|e| SecretsError::Decrypt(format!("base64: {e}")))?;
    if raw.len() < BLOB_PREFIX.len() + NONCE_LEN + 16 {
        return Err(SecretsError::Decrypt("blob too short".into()));
    }
    if &raw[..BLOB_PREFIX.len()] != BLOB_PREFIX {
        return Err(SecretsError::Decrypt("unknown blob version".into()));
    }
    let nonce_start = BLOB_PREFIX.len();
    let ct_start = nonce_start + NONCE_LEN;
    let nonce = Nonce::try_from(&raw[nonce_start..ct_start])
        .map_err(|e| SecretsError::Decrypt(format!("nonce: {e}")))?;
    let key = load_or_create_master_key(&master_key_path())?;
    let cipher = cipher(&key);
    cipher
        .decrypt(&nonce, &raw[ct_start..])
        .map_err(|e| SecretsError::Decrypt(e.to_string()))
}

pub fn seal_mtls(bundle: &OpenShellMtlsBundle) -> Result<String, SecretsError> {
    bundle.validate_pem_shape()?;
    let json = serde_json::to_vec(bundle)?;
    seal(&json)
}

/// Seal a string map (provider credentials / refresh material) as JSON.
pub fn seal_string_map(map: &std::collections::BTreeMap<String, String>) -> Result<String, SecretsError> {
    let json = serde_json::to_vec(map)?;
    seal(&json)
}

/// Open a blob produced by [`seal_string_map`].
pub fn open_string_map(
    sealed_b64: &str,
) -> Result<std::collections::BTreeMap<String, String>, SecretsError> {
    let plain = open(sealed_b64)?;
    Ok(serde_json::from_slice(&plain)?)
}

pub fn open_mtls(sealed_b64: &str) -> Result<OpenShellMtlsBundle, SecretsError> {
    let plain = open(sealed_b64)?;
    let bundle: OpenShellMtlsBundle = serde_json::from_slice(&plain)?;
    Ok(bundle)
}

pub fn mtls_status_from_sealed(sealed: Option<&str>) -> OpenShellMtlsStatus {
    match sealed.map(str::trim).filter(|s| !s.is_empty()) {
        None => OpenShellMtlsStatus::default(),
        Some(s) => match open_mtls(s) {
            Ok(b) => OpenShellMtlsStatus::from(&b),
            Err(_) => OpenShellMtlsStatus {
                // Sealed blob present but unreadable (wrong master key) —
                // surface as incomplete so the operator re-uploads.
                ca: false,
                client_cert: false,
                client_key: false,
                complete: false,
            },
        },
    }
}

pub fn seal_oidc(bundle: &OpenShellOidcBundle) -> Result<String, SecretsError> {
    bundle.validate_for_seal()?;
    let json = serde_json::to_vec(bundle)?;
    seal(&json)
}

pub fn open_oidc(sealed_b64: &str) -> Result<OpenShellOidcBundle, SecretsError> {
    let plain = open(sealed_b64)?;
    let bundle: OpenShellOidcBundle = serde_json::from_slice(&plain)?;
    Ok(bundle)
}

pub fn oidc_status_from_sealed(sealed: Option<&str>) -> OpenShellOidcStatus {
    match sealed.map(str::trim).filter(|s| !s.is_empty()) {
        None => OpenShellOidcStatus::default(),
        Some(s) => match open_oidc(s) {
            Ok(b) => OpenShellOidcStatus {
                logged_in: b.validate_for_seal().is_ok(),
            },
            // Blob present but unreadable — treat as logged out so the
            // operator re-auths rather than thinking they're still in.
            Err(_) => OpenShellOidcStatus { logged_in: false },
        },
    }
}

/// Seal a legacy board-level App blob (migration fixtures / round-trip tests).
/// Live material is sealed on the `github-app` provider credentials map.
#[cfg_attr(not(test), allow(dead_code))]
pub fn seal_github_app(bundle: &GitHubAppBundle) -> Result<String, SecretsError> {
    bundle.validate_for_seal()?;
    let json = serde_json::to_vec(bundle)?;
    seal(&json)
}

pub fn open_github_app(sealed_b64: &str) -> Result<GitHubAppBundle, SecretsError> {
    let plain = open(sealed_b64)?;
    let bundle: GitHubAppBundle = serde_json::from_slice(&plain)?;
    Ok(bundle)
}

pub fn github_app_status_from_sealed(sealed: Option<&str>) -> GitHubAppStatus {
    match sealed.map(str::trim).filter(|s| !s.is_empty()) {
        None => GitHubAppStatus::default(),
        Some(s) => match open_github_app(s) {
            Ok(b) => GitHubAppStatus::from(&b),
            Err(_) => GitHubAppStatus::default(),
        },
    }
}

/// Decrypt for public fields + status. `None` when unset or unreadable.
pub fn github_app_view_from_sealed(sealed: Option<&str>) -> Option<GitHubAppBundle> {
    sealed
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| open_github_app(s).ok())
}

pub fn seal_auth(bundle: &AuthBundle) -> Result<String, SecretsError> {
    if !bundle.is_configured() {
        return Err(SecretsError::Encrypt(
            "auth: admin_username and password_hash required".into(),
        ));
    }
    if bundle.session_key_b64.trim().is_empty() {
        return Err(SecretsError::Encrypt("auth: session_key required".into()));
    }
    let json = serde_json::to_vec(bundle)?;
    seal(&json)
}

pub fn open_auth(sealed_b64: &str) -> Result<AuthBundle, SecretsError> {
    let plain = open(sealed_b64)?;
    Ok(serde_json::from_slice(&plain)?)
}

pub fn auth_from_sealed(sealed: Option<&str>) -> Option<AuthBundle> {
    sealed
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| open_auth(s).ok())
        .filter(|b| b.is_configured())
}

/// Serialize + restore `SANDBOARD_MASTER_KEY*` across tests (process-global env).
#[cfg(test)]
pub(crate) mod master_key_env {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct Guard {
        _lock: MutexGuard<'static, ()>,
        prev_path: Option<String>,
        prev_hex: Option<String>,
    }

    impl Guard {
        fn take_lock() -> MutexGuard<'static, ()> {
            LOCK.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn capture() -> (Option<String>, Option<String>) {
            (
                std::env::var("SANDBOARD_MASTER_KEY_PATH").ok(),
                std::env::var("SANDBOARD_MASTER_KEY").ok(),
            )
        }

        /// Exclusive use of a file-backed master key path.
        pub(crate) fn with_key_path(path: impl AsRef<Path>) -> Self {
            let _lock = Self::take_lock();
            let (prev_path, prev_hex) = Self::capture();
            std::env::set_var("SANDBOARD_MASTER_KEY_PATH", path.as_ref());
            std::env::remove_var("SANDBOARD_MASTER_KEY");
            Self {
                _lock,
                prev_path,
                prev_hex,
            }
        }

        /// Exclusive use of `SANDBOARD_MASTER_KEY` hex (no path override).
        pub(crate) fn with_hex_key(hex: &str) -> Self {
            let _lock = Self::take_lock();
            let (prev_path, prev_hex) = Self::capture();
            std::env::remove_var("SANDBOARD_MASTER_KEY_PATH");
            std::env::set_var("SANDBOARD_MASTER_KEY", hex);
            Self {
                _lock,
                prev_path,
                prev_hex,
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            match &self.prev_path {
                Some(p) => std::env::set_var("SANDBOARD_MASTER_KEY_PATH", p),
                None => std::env::remove_var("SANDBOARD_MASTER_KEY_PATH"),
            }
            match &self.prev_hex {
                Some(h) => std::env::set_var("SANDBOARD_MASTER_KEY", h),
                None => std::env::remove_var("SANDBOARD_MASTER_KEY"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bundle() -> OpenShellMtlsBundle {
        OpenShellMtlsBundle {
            ca_pem: "-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n".into(),
            client_cert_pem: "-----BEGIN CERTIFICATE-----\nCERT\n-----END CERTIFICATE-----\n"
                .into(),
            client_key_pem: "-----BEGIN PRIVATE KEY-----\nKEY\n-----END PRIVATE KEY-----\n"
                .into(),
        }
    }

    #[test]
    fn seal_open_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-secrets-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = master_key_env::Guard::with_key_path(&key_path);

        let bundle = sample_bundle();
        let sealed = seal_mtls(&bundle).expect("seal");
        assert!(!sealed.contains("BEGIN"));
        let opened = open_mtls(&sealed).expect("open");
        assert_eq!(opened, bundle);
        assert!(key_path.exists());

        drop(_env);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_env_master_key() {
        let hex = "aa".repeat(KEY_LEN);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let sealed = seal(b"hello").expect("seal");
        let plain = open(&sealed).expect("open");
        assert_eq!(plain, b"hello");
    }

    #[test]
    fn rejects_non_pem() {
        let b = OpenShellMtlsBundle {
            ca_pem: "/tmp/ca.crt".into(),
            client_cert_pem: sample_bundle().client_cert_pem,
            client_key_pem: sample_bundle().client_key_pem,
        };
        assert!(b.validate_pem_shape().is_err());
    }

    #[test]
    fn seal_string_map_round_trip() {
        let hex = "bb".repeat(KEY_LEN);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let mut map = std::collections::BTreeMap::new();
        map.insert("GITHUB_TOKEN".into(), "ghp_secret_value".into());
        map.insert("OTHER".into(), "x".into());
        let sealed = seal_string_map(&map).expect("seal");
        assert!(!sealed.contains("ghp_secret_value"));
        assert_eq!(open_string_map(&sealed).expect("open"), map);
    }

    #[test]
    fn seal_github_app_round_trip() {
        let hex = "cc".repeat(KEY_LEN);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let bundle = GitHubAppBundle {
            app_id: "123456".into(),
            private_key_pem: "-----BEGIN RSA PRIVATE KEY-----\nKEY\n-----END RSA PRIVATE KEY-----\n"
                .into(),
            webhook_secret: "whsec_test".into(),
            client_id: "Iv1.abc".into(),
            client_secret: "cs_secret".into(),
        };
        let sealed = seal_github_app(&bundle).expect("seal");
        assert!(!sealed.contains("BEGIN"));
        assert!(!sealed.contains("whsec_test"));
        assert!(!sealed.contains("cs_secret"));
        assert_eq!(open_github_app(&sealed).expect("open"), bundle);
        let st = github_app_status_from_sealed(Some(&sealed));
        assert!(st.complete);
        assert!(st.webhook_secret);
    }

    #[test]
    fn github_app_rejects_missing_key() {
        let b = GitHubAppBundle {
            app_id: "1".into(),
            private_key_pem: String::new(),
            ..Default::default()
        };
        assert!(b.validate_for_seal().is_err());
    }

    #[test]
    fn seal_auth_round_trip() {
        let hex = "dd".repeat(KEY_LEN);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let bundle = AuthBundle {
            admin_username: "admin".into(),
            password_hash: "$argon2id$v=19$m=16,t=2,p=1$test$testhash".into(),
            session_key_b64: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [7u8; 32],
            ),
        };
        let sealed = seal_auth(&bundle).expect("seal");
        assert!(!sealed.contains("admin"));
        assert_eq!(open_auth(&sealed).expect("open"), bundle);
        assert!(auth_from_sealed(Some(&sealed)).is_some());
    }
}
