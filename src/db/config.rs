//! Board database URL config (compiled default / `SANDBOARD_DATABASE_URL`).

use serde::{Deserialize, Serialize};

/// Env override for `board.database.url`. Matches the `SANDBOARD_*` pattern used for
/// the listen port.
pub const ENV_DATABASE_URL: &str = "SANDBOARD_DATABASE_URL";

/// Default when yaml omits the key — local SQLite beside the process cwd.
pub const DEFAULT_DATABASE_URL: &str = "sqlite:sandboard.db";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseBackend {
    Sqlite,
    Postgres,
}

impl std::fmt::Display for DatabaseBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DatabaseBackend::Sqlite => write!(f, "sqlite"),
            DatabaseBackend::Postgres => write!(f, "postgres"),
        }
    }
}

/// A validated board database URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseUrl {
    raw: String,
    backend: DatabaseBackend,
}

impl DatabaseUrl {
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn backend(&self) -> DatabaseBackend {
        self.backend
    }
}

impl std::fmt::Display for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// Parse `sqlite:…` / `sqlite://…` and `postgres://…` / `postgresql://…`.
pub fn parse_database_url(raw: &str) -> Result<DatabaseUrl, ParseDatabaseUrlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ParseDatabaseUrlError::Empty);
    }
    let backend = if trimmed.starts_with("sqlite:") {
        DatabaseBackend::Sqlite
    } else if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
        DatabaseBackend::Postgres
    } else {
        return Err(ParseDatabaseUrlError::UnsupportedScheme {
            url: trimmed.to_string(),
        });
    };
    Ok(DatabaseUrl {
        raw: trimmed.to_string(),
        backend,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseDatabaseUrlError {
    #[error("board database URL is empty")]
    Empty,
    #[error(
        "unsupported board database URL {url:?}; expected sqlite:… or postgres://… / postgresql://…"
    )]
    UnsupportedScheme { url: String },
}

/// Board database URL (process boot).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardDatabaseConfig {
    /// SQLx URL. Default `sqlite:sandboard.db`. Override with `SANDBOARD_DATABASE_URL`.
    #[serde(default = "default_url")]
    pub url: String,
}

fn default_url() -> String {
    DEFAULT_DATABASE_URL.to_string()
}

impl Default for BoardDatabaseConfig {
    fn default() -> Self {
        Self { url: default_url() }
    }
}

impl BoardDatabaseConfig {
    /// Parsed, validated URL from the configured string.
    pub fn parsed(&self) -> Result<DatabaseUrl, ParseDatabaseUrlError> {
        parse_database_url(&self.url)
    }
}

/// Prefer `SANDBOARD_DATABASE_URL` when set; otherwise keep the compiled default.
pub fn apply_database_url_override(cfg: &mut BoardDatabaseConfig) {
    if let Ok(url) = std::env::var(ENV_DATABASE_URL) {
        let url = url.trim();
        if !url.is_empty() {
            cfg.url = url.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sqlite_forms() {
        let u = parse_database_url("sqlite:sandboard.db").unwrap();
        assert_eq!(u.backend(), DatabaseBackend::Sqlite);
        assert_eq!(u.as_str(), "sqlite:sandboard.db");

        let mem = parse_database_url("sqlite::memory:").unwrap();
        assert_eq!(mem.backend(), DatabaseBackend::Sqlite);

        let slash = parse_database_url("sqlite://localhost/tmp/sandboard.db").unwrap();
        assert_eq!(slash.backend(), DatabaseBackend::Sqlite);
    }

    #[test]
    fn parses_postgres_forms() {
        let u = parse_database_url("postgres://sandboard:sandboard@localhost:5432/sandboard").unwrap();
        assert_eq!(u.backend(), DatabaseBackend::Postgres);

        let u2 = parse_database_url("postgresql://localhost/sandboard").unwrap();
        assert_eq!(u2.backend(), DatabaseBackend::Postgres);
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert!(matches!(
            parse_database_url(""),
            Err(ParseDatabaseUrlError::Empty)
        ));
        assert!(matches!(
            parse_database_url("mysql://localhost/sandboard"),
            Err(ParseDatabaseUrlError::UnsupportedScheme { .. })
        ));
    }

    #[test]
    fn yaml_defaults_to_sqlite() {
        let cfg: BoardDatabaseConfig = serde_yaml::from_str("url: sqlite:sandboard.db").unwrap();
        assert_eq!(cfg.parsed().unwrap().backend(), DatabaseBackend::Sqlite);

        let empty: BoardDatabaseConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(empty.url, DEFAULT_DATABASE_URL);
        assert_eq!(empty.parsed().unwrap().backend(), DatabaseBackend::Sqlite);
    }
}
