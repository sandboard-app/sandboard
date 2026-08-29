//! Board-owned OpenShell provider type profiles.
//!
//! Shipped YAML (antigravity, cursor-agent, github-app) seeds the board catalog;
//! Sync imports every board type to the gateway before applying provider instances.
//! Builtin OpenShell types (including egress-only `cursor`) stay on the gateway.

use crate::model::{
    OpenShellProviderTypeDesired, ANTIGRAVITY_PROVIDER, CURSOR_AGENT_PROVIDER_TYPE,
    GITHUB_APP_PROVIDER_TYPE, OPENROUTER_PROVIDER_TYPE,
};
use crate::openshell::{OpenShell, ProviderTypeProfile};
use crate::store::{Board, SharedBoard};

const ANTIGRAVITY_YAML: &str = include_str!("../sandbox/openshell/antigravity.yaml");
const CURSOR_AGENT_YAML: &str = include_str!("../sandbox/openshell/cursor-agent.yaml");
const GITHUB_APP_YAML: &str = include_str!("../sandbox/openshell/github-app.yaml");
const OPENROUTER_YAML: &str = include_str!("../sandbox/openshell/openrouter.yaml");

/// Parsed metadata used by API upsert and catalog merge.
#[derive(Debug, Clone)]
pub struct ParsedProviderType {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub credential_env_vars: Vec<String>,
}

/// Shipped board types seeded when missing (and not tombstoned).
pub fn shipped_provider_types() -> Vec<OpenShellProviderTypeDesired> {
    vec![
        OpenShellProviderTypeDesired {
            id: ANTIGRAVITY_PROVIDER.into(),
            yaml: ANTIGRAVITY_YAML.trim().to_string(),
            shipped: true,
            form_config_keys: vec![
                "ANTIGRAVITY_GCP_PROJECT".into(),
                "ANTIGRAVITY_GCP_LOCATION".into(),
            ],
        },
        OpenShellProviderTypeDesired {
            id: CURSOR_AGENT_PROVIDER_TYPE.into(),
            yaml: CURSOR_AGENT_YAML.trim().to_string(),
            shipped: true,
            form_config_keys: vec![],
        },
        OpenShellProviderTypeDesired {
            id: OPENROUTER_PROVIDER_TYPE.into(),
            yaml: OPENROUTER_YAML.trim().to_string(),
            shipped: true,
            form_config_keys: vec![],
        },
        OpenShellProviderTypeDesired {
            id: GITHUB_APP_PROVIDER_TYPE.into(),
            yaml: GITHUB_APP_YAML.trim().to_string(),
            shipped: true,
            form_config_keys: vec![
                crate::github_app::CONFIG_APP_ID.into(),
                crate::github_app::CONFIG_INSTALLATION_ID.into(),
            ],
        },
    ]
}

/// Insert missing shipped types. Returns how many were added.
pub fn ensure_shipped_on_board(board: &Board) -> usize {
    board.ensure_shipped_provider_types(shipped_provider_types())
}

/// Parse and validate provider-type YAML. `expected_id` must match document `id`.
pub fn parse_provider_type_yaml(
    yaml: &str,
    expected_id: Option<&str>,
) -> Result<ParsedProviderType, String> {
    let yaml = yaml.trim();
    if yaml.is_empty() {
        return Err("provider type yaml required".into());
    }

    // Same parser Sync uses on import — reject documents the gateway won't accept.
    let profile =
        openshell_providers::parse_profile_yaml(yaml).map_err(|e| format!("invalid provider type yaml: {e}"))?;
    let mut credential_env_vars: Vec<String> = profile
        .credential_env_vars()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    credential_env_vars.sort();
    credential_env_vars.dedup();
    let parsed = ParsedProviderType {
        id: profile.id,
        display_name: profile.display_name,
        description: profile.description,
        credential_env_vars,
    };

    if parsed.id.is_empty() {
        return Err("provider type yaml missing id".into());
    }
    if let Some(expected) = expected_id.map(str::trim).filter(|s| !s.is_empty()) {
        if parsed.id != expected {
            return Err(format!(
                "yaml id {:?} does not match request id {expected:?}",
                parsed.id
            ));
        }
    }
    Ok(parsed)
}

/// Reconcile every board custom type with the gateway.
pub async fn import_board_types_to_gateway(
    board: &SharedBoard,
    os: &OpenShell,
) -> Result<(), String> {
    let types = board.openshell_provider_types();
    for entry in types.values() {
        os.upsert_provider_type_yaml(&entry.id, &entry.yaml)
            .await
            .map_err(|e| format!("reconcile provider type {}: {e}", entry.id))?;
    }
    Ok(())
}

/// Reconcile only the custom provider types needed by one sandbox.
///
/// Dispatch must not be blocked by an unrelated provider profile that is
/// invalid on the local gateway; built-in provider types are already owned by
/// OpenShell and are intentionally skipped when Board has no custom entry.
pub async fn import_attached_provider_types_to_gateway(
    board: &SharedBoard,
    os: &OpenShell,
    provider_names: &[String],
) -> Result<(), String> {
    let desired = board.openshell_providers();
    let types = board.openshell_provider_types();
    let mut type_ids = std::collections::BTreeSet::new();
    for name in provider_names {
        let provider = desired
            .iter()
            .find(|p| p.name == *name)
            .ok_or_else(|| format!("provider {name:?} is not in Board desired state"))?;
        type_ids.insert(provider.provider_type.clone());
    }
    for type_id in type_ids {
        let Some(entry) = types.values().find(|entry| entry.id == type_id) else {
            continue;
        };
        os.upsert_provider_type_yaml(&entry.id, &entry.yaml)
            .await
            .map_err(|e| format!("reconcile provider type {}: {e}", entry.id))?;
    }
    Ok(())
}

/// Merge board customs with gateway-live profiles for Settings.
pub fn merge_catalog(
    board_types: &std::collections::BTreeMap<String, OpenShellProviderTypeDesired>,
    gateway: &[ProviderTypeProfile],
) -> Vec<ProviderTypeCatalogEntry> {
    let mut by_id: std::collections::BTreeMap<String, ProviderTypeCatalogEntry> =
        std::collections::BTreeMap::new();

    for g in gateway {
        by_id.insert(
            g.id.clone(),
            ProviderTypeCatalogEntry {
                id: g.id.clone(),
                display_name: if g.display_name.trim().is_empty() {
                    g.id.clone()
                } else {
                    g.display_name.clone()
                },
                description: g.description.clone(),
                source: "builtin".into(),
                credential_env_vars: g.credential_env_vars.clone(),
                form_config_keys: Vec::new(),
                yaml: None,
                shipped: None,
            },
        );
    }

    for entry in board_types.values() {
        let meta = parse_provider_type_yaml(&entry.yaml, Some(&entry.id)).ok();
        let display_name = meta
            .as_ref()
            .map(|m| {
                if m.display_name.trim().is_empty() {
                    m.id.clone()
                } else {
                    m.display_name.clone()
                }
            })
            .unwrap_or_else(|| entry.id.clone());
        let description = meta
            .as_ref()
            .map(|m| m.description.clone())
            .unwrap_or_default();
        let credential_env_vars = meta
            .as_ref()
            .map(|m| m.credential_env_vars.clone())
            .unwrap_or_default();

        match by_id.get_mut(&entry.id) {
            Some(existing) => {
                existing.source = "both".into();
                existing.display_name = display_name;
                existing.description = description;
                if !credential_env_vars.is_empty() {
                    existing.credential_env_vars = credential_env_vars;
                }
                existing.form_config_keys = entry.form_config_keys.clone();
                existing.yaml = Some(entry.yaml.clone());
                existing.shipped = Some(entry.shipped);
            }
            None => {
                by_id.insert(
                    entry.id.clone(),
                    ProviderTypeCatalogEntry {
                        id: entry.id.clone(),
                        display_name,
                        description,
                        source: "board".into(),
                        credential_env_vars,
                        form_config_keys: entry.form_config_keys.clone(),
                        yaml: Some(entry.yaml.clone()),
                        shipped: Some(entry.shipped),
                    },
                );
            }
        }
    }

    by_id.into_values().collect()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderTypeCatalogEntry {
    pub id: String,
    pub display_name: String,
    pub description: String,
    /// `board`, `builtin`, or `both`.
    pub source: String,
    pub credential_env_vars: Vec<String>,
    pub form_config_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yaml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipped: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Board;
    use std::sync::Arc;

    fn temp_board() -> Arc<Board> {
        let path = std::env::temp_dir().join(format!(
            "sandboard-provider-types-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Arc::new(Board::new(crate::schema::Schema::default(), path))
    }

    #[test]
    fn shipped_yaml_parses_with_expected_ids() {
        let agy = parse_provider_type_yaml(ANTIGRAVITY_YAML, Some(ANTIGRAVITY_PROVIDER)).unwrap();
        assert_eq!(agy.id, ANTIGRAVITY_PROVIDER);
        assert!(agy
            .credential_env_vars
            .iter()
            .any(|e| e == "ANTIGRAVITY_ACCESS_TOKEN"));

        let cursor =
            parse_provider_type_yaml(CURSOR_AGENT_YAML, Some(CURSOR_AGENT_PROVIDER_TYPE)).unwrap();
        assert_eq!(cursor.id, CURSOR_AGENT_PROVIDER_TYPE);
        assert!(cursor
            .credential_env_vars
            .iter()
            .any(|e| e == "CURSOR_API_KEY"));
        let cursor_profile = openshell_providers::parse_profile_yaml(CURSOR_AGENT_YAML).unwrap();
        assert_eq!(
            cursor_profile
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.allow_uninspected_credentials)
                .count(),
            4
        );

        let gh =
            parse_provider_type_yaml(GITHUB_APP_YAML, Some(GITHUB_APP_PROVIDER_TYPE)).unwrap();
        assert_eq!(gh.id, GITHUB_APP_PROVIDER_TYPE);
        assert!(gh.credential_env_vars.iter().any(|e| e == "GH_TOKEN"));

        let openrouter = parse_provider_type_yaml(
            OPENROUTER_YAML,
            Some(OPENROUTER_PROVIDER_TYPE),
        )
        .unwrap();
        assert_eq!(openrouter.id, OPENROUTER_PROVIDER_TYPE);
        assert!(openrouter
            .credential_env_vars
            .iter()
            .any(|e| e == "OPENROUTER_API_KEY"));
    }

    #[test]
    fn ensure_seeds_missing_shipped_types() {
        let b = temp_board();
        assert!(b.openshell_provider_types().is_empty());
        let added = ensure_shipped_on_board(&b);
        assert_eq!(added, 4);
        let types = b.openshell_provider_types();
        assert!(types.contains_key(ANTIGRAVITY_PROVIDER));
        assert!(types.contains_key(CURSOR_AGENT_PROVIDER_TYPE));
        assert!(types.contains_key(GITHUB_APP_PROVIDER_TYPE));
        assert!(types.contains_key(OPENROUTER_PROVIDER_TYPE));
        assert_eq!(ensure_shipped_on_board(&b), 0);
    }

    #[test]
    fn delete_tombstone_blocks_reseed() {
        let b = temp_board();
        ensure_shipped_on_board(&b);
        assert!(b.delete_openshell_provider_type(CURSOR_AGENT_PROVIDER_TYPE));
        assert!(!b
            .openshell_provider_types()
            .contains_key(CURSOR_AGENT_PROVIDER_TYPE));
        assert_eq!(ensure_shipped_on_board(&b), 0);
        assert!(!b
            .openshell_provider_types()
            .contains_key(CURSOR_AGENT_PROVIDER_TYPE));
        // antigravity still present
        assert!(b
            .openshell_provider_types()
            .contains_key(ANTIGRAVITY_PROVIDER));
    }

    #[test]
    fn upsert_rejects_id_mismatch() {
        let err = parse_provider_type_yaml(CURSOR_AGENT_YAML, Some("wrong-id")).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[tokio::test]
    async fn import_calls_gateway_for_each_board_type() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                if args.first().map(String::as_str) == Some("provider")
                    && args.get(2).map(String::as_str) == Some("import")
                {
                    seen2
                        .lock()
                        .unwrap()
                        .push(args.get(3).cloned().unwrap_or_default());
                }
                crate::openshell::Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );
        let b = temp_board();
        ensure_shipped_on_board(&b);
        import_board_types_to_gateway(&b, &os).await.unwrap();
        let got = seen.lock().unwrap().clone();
        assert!(
            got.iter().any(|s| s.contains("antigravity")),
            "expected antigravity import, got {got:?}"
        );
        assert!(
            got.iter().any(|s| s.contains("cursor-agent")),
            "expected cursor-agent import, got {got:?}"
        );
        assert!(
            got.iter().any(|s| s.contains("github-app")),
            "expected github-app import, got {got:?}"
        );
        assert!(
            got.iter()
                .any(|s| s.contains(OPENROUTER_PROVIDER_TYPE)),
            "expected openrouter import, got {got:?}"
        );
    }

    #[tokio::test]
    async fn attached_import_skips_unrelated_provider_types() {
        use crate::model::OpenShellProviderDesired;
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                if args.first().map(String::as_str) == Some("provider")
                    && args.get(2).map(String::as_str) == Some("import")
                {
                    seen2
                        .lock()
                        .unwrap()
                        .push(args.get(3).cloned().unwrap_or_default());
                }
                crate::openshell::Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );
        let b = temp_board();
        ensure_shipped_on_board(&b);
        b.upsert_openshell_provider(
            OpenShellProviderDesired {
                name: "hermes".into(),
                provider_type: OPENROUTER_PROVIDER_TYPE.into(),
                config: Default::default(),
                credentials_sealed: None,
                credential_keys: vec!["OPENROUTER_API_KEY".into()],
                refresh: None,
            }
            .normalized(),
        );

        import_attached_provider_types_to_gateway(
            &b,
            &os,
            &["hermes".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[OPENROUTER_PROVIDER_TYPE.to_string()]
        );
    }

    #[tokio::test]
    async fn import_updates_existing_board_type() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                seen2.lock().unwrap().push(args.to_vec());
                if args.get(2).map(String::as_str) == Some("import") {
                    return crate::openshell::Output {
                        code: 1,
                        stdout: String::new(),
                        stderr: "profile already exists".into(),
                    };
                }
                crate::openshell::Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );
        let b = temp_board();
        ensure_shipped_on_board(&b);

        import_board_types_to_gateway(&b, &os).await.unwrap();

        let calls = seen.lock().unwrap();
        assert!(
            calls.iter().any(|args| {
                args.get(2).map(String::as_str) == Some("update")
                    && args.iter().any(|arg| arg.contains("cursor-agent"))
            }),
            "expected an update after import conflict, got {calls:?}"
        );
    }

    #[test]
    fn merge_marks_board_and_builtin() {
        let mut board = std::collections::BTreeMap::new();
        board.insert(
            CURSOR_AGENT_PROVIDER_TYPE.into(),
            OpenShellProviderTypeDesired {
                id: CURSOR_AGENT_PROVIDER_TYPE.into(),
                yaml: CURSOR_AGENT_YAML.trim().into(),
                shipped: true,
                form_config_keys: vec![],
            },
        );
        let gateway = vec![ProviderTypeProfile {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            description: "egress".into(),
            category: "agent".into(),
            credential_env_vars: vec![],
            config_keys: vec![],
        }];
        let merged = merge_catalog(&board, &gateway);
        let cursor_builtin = merged.iter().find(|e| e.id == "cursor").unwrap();
        assert_eq!(cursor_builtin.source, "builtin");
        let cursor_agent = merged
            .iter()
            .find(|e| e.id == CURSOR_AGENT_PROVIDER_TYPE)
            .unwrap();
        assert_eq!(cursor_agent.source, "board");
        assert!(cursor_agent
            .credential_env_vars
            .iter()
            .any(|e| e == "CURSOR_API_KEY"));
    }
}
