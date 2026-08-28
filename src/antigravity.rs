//! Antigravity (`agy`) OpenShell provider type helpers.
//!
//! The seat must never see a host OAuth file. The gateway holds the live
//! access token; the sandbox only gets an `openshell:resolve:…` placeholder
//! via provider type `antigravity` (Bearer on Cloud Code endpoints).
//!
//! The token arrives over the API like every other credential. sandboard does not
//! read the host keychain: reaching into a developer's credential store is a
//! guess about the machine sandboard happens to be running on, and it silently
//! adopted tokens that were put there for something else entirely.
//!
//! Profile YAML lives in the board provider-type catalog
//! ([`crate::provider_types`]); Sync imports it with every other board type.

use crate::model::ANTIGRAVITY_PROVIDER;
use crate::store::SharedBoard;

/// Board provider config keys (Settings → Providers → antigravity).
/// Written into seat `settings.json` by [`crate::supervisor::setup_agy_auth`].
pub const CONFIG_PROJECT: &str = "ANTIGRAVITY_GCP_PROJECT";
pub const CONFIG_LOCATION: &str = "ANTIGRAVITY_GCP_LOCATION";

/// Seat default model. Put `--model` **before** `-p` — `-p` consumes the next
/// argv as the prompt.
///
/// Pair with consumer-client OAuth (`auth_method: gcp` + board project).
/// Business-client tokens leave this label without `vertexModelId`.
pub const DEFAULT_SEAT_MODEL: &str = "gemini-3.6-flash-high";

/// GCP project/location from Board provider config (never host files).
pub fn gcp_from_board(board: &SharedBoard) -> Result<(String, String), String> {
    let provider = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == ANTIGRAVITY_PROVIDER)
        .ok_or_else(|| {
            "no Board provider `antigravity` — add it under Settings → Providers".to_string()
        })?;
    let project = provider
        .config
        .get(CONFIG_PROJECT)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("antigravity provider missing config `{CONFIG_PROJECT}` (Settings → Providers)")
        })?;
    let location = provider
        .config
        .get(CONFIG_LOCATION)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("global");
    Ok((project.to_string(), location.to_string()))
}

/// Shell exports so agy uses the antigravity provider project, not Vertex's
/// `GOOGLE_CLOUD_PROJECT` (cockpit often attaches both). Empty when Board
/// config is incomplete — caller skips.
///
/// Agy leaves `quotaProject` empty unless these are set; `settings.json`
/// `gcp.project` alone is not enough.
pub fn cloud_env_exports(board: &SharedBoard) -> String {
    let Ok((project, location)) = gcp_from_board(board) else {
        return String::new();
    };
    let p = shell_single_quote(&project);
    let l = shell_single_quote(&location);
    format!(
        "export GOOGLE_CLOUD_PROJECT={p}\n\
         export GOOGLE_CLOUD_QUOTA_PROJECT={p}\n\
         export GCP_PROJECT_ID={p}\n\
         export GCP_LOCATION={l}\n\
         export CLOUD_ML_REGION={l}\n\
         export VERTEX_LOCATION={l}\n"
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Attach `antigravity` to the running cockpit sandbox when the cockpit
/// create-spec lists it.
pub async fn attach_to_running_cockpit(board: &SharedBoard) -> Result<(), String> {
    let resolved = board.resolve_cockpit_sandbox_create();
    if !resolved
        .providers
        .iter()
        .any(|n| n == ANTIGRAVITY_PROVIDER)
    {
        return Ok(());
    }
    let Some(session) = board.cockpit_session() else {
        return Ok(());
    };
    if session.status != crate::model::CockpitSessionStatus::Running {
        return Ok(());
    }
    let Some(env) = session
        .environment
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let os = board.openshell_client();
    os.attach_sandbox_provider(env, ANTIGRAVITY_PROVIDER)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OpenShellProviderDesired;
    use std::collections::BTreeMap;

    #[test]
    fn gcp_from_board_reads_provider_config() {
        use crate::store::Board;
        use std::sync::Arc;
        let path = std::env::temp_dir().join(format!(
            "sandboard-agy-gcp-board-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let board = Arc::new(Board::new(crate::schema::Schema::default(), path));
        let mut config = BTreeMap::new();
        config.insert(CONFIG_PROJECT.into(), "my-gcp".into());
        config.insert(CONFIG_LOCATION.into(), "us-central1".into());
        board.upsert_openshell_provider(OpenShellProviderDesired {
            name: ANTIGRAVITY_PROVIDER.into(),
            provider_type: ANTIGRAVITY_PROVIDER.into(),
            config,
            credentials_sealed: None,
            credential_keys: vec![],
            refresh: None,
        });
        let (project, location) = gcp_from_board(&board).unwrap();
        assert_eq!(project, "my-gcp");
        assert_eq!(location, "us-central1");
        let exports = cloud_env_exports(&board);
        assert!(exports.contains("GOOGLE_CLOUD_PROJECT='my-gcp'"), "{exports}");
        assert!(
            exports.contains("GOOGLE_CLOUD_QUOTA_PROJECT='my-gcp'"),
            "{exports}"
        );
    }
}
