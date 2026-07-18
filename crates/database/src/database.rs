//! Domain types shared by the database viewer UI and its JDBC sidecar.

use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};
use thiserror::Error;
use uuid::Uuid;

mod driver_manager;
mod sidecar;
mod sql;

pub use driver_manager::{
    JdbcDriverDownload, download_jdbc_driver, installed_jdbc_driver_path, resolve_jdbc_driver_path,
};
pub use sidecar::{
    ConnectionTestResult, MetadataColumn, MetadataDatabase, MetadataIndex, MetadataTable,
    QueryColumn, QueryResult, TableMetadataDetails, browse_table, describe_table, execute_query,
    list_databases, list_tables, test_connection,
};
pub use sql::{split_sql_statements, sql_statement_at_offset};

/// Version of the serialized connection registry.
pub const CONNECTION_REGISTRY_VERSION: u32 = 1;
pub const CONSOLE_REGISTRY_VERSION: u32 = 1;

/// Stable identity for a saved database connection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(Uuid);

impl ConnectionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn credential_key(self) -> String {
        format!("zed-database://connections/{self}")
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable identity for a workspace-local SQL console.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConsoleId(Uuid);

impl ConsoleId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConsoleId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConsoleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Drivers supported by the first database viewer milestone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DatabaseDriver {
    #[serde(rename = "postgresql", alias = "postgre_sql")]
    PostgreSql,
    #[serde(rename = "mysql", alias = "my_sql")]
    MySql,
    #[serde(rename = "clickhouse", alias = "click_house")]
    ClickHouse,
    #[serde(rename = "sqlite")]
    Sqlite,
    #[serde(rename = "custom")]
    Custom,
}

impl DatabaseDriver {
    pub const ALL: [Self; 5] = [
        Self::PostgreSql,
        Self::MySql,
        Self::ClickHouse,
        Self::Sqlite,
        Self::Custom,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::PostgreSql => "PostgreSQL",
            Self::MySql => "MySQL",
            Self::ClickHouse => "ClickHouse",
            Self::Sqlite => "SQLite",
            Self::Custom => "Custom JDBC",
        }
    }

    pub fn default_jdbc_url(self) -> &'static str {
        match self {
            Self::PostgreSql => "jdbc:postgresql://localhost:5432/postgres",
            Self::MySql => "jdbc:mysql://localhost:3306/mysql",
            Self::ClickHouse => "jdbc:clickhouse://localhost:8123/default",
            Self::Sqlite => "jdbc:sqlite:database.sqlite",
            Self::Custom => "jdbc:",
        }
    }

    pub fn download(self) -> Option<JdbcDriverDownload> {
        driver_manager::download_for_driver(self)
    }
}

impl fmt::Display for DatabaseDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.display_name())
    }
}

/// Scope controlling where a connection is visible.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionScope {
    #[default]
    Global,
    Project,
}

/// Non-secret settings for a saved connection.
///
/// Passwords and tokens deliberately do not belong here. The UI will store them
/// through Zed's credential provider and send them to the sidecar only while
/// opening a connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    pub driver: DatabaseDriver,
    pub jdbc_url: String,
    pub username: Option<String>,
    #[serde(default)]
    pub custom_driver_path: Option<PathBuf>,
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    #[serde(default)]
    pub scope: ConnectionScope,
}

fn default_read_only() -> bool {
    true
}

impl ConnectionProfile {
    pub fn new(name: impl Into<String>, driver: DatabaseDriver) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            driver,
            jdbc_url: driver.default_jdbc_url().to_owned(),
            username: None,
            custom_driver_path: None,
            read_only: true,
            scope: ConnectionScope::Global,
        }
    }

    pub fn validate(&self) -> Result<(), ConnectionProfileError> {
        if self.name.trim().is_empty() {
            return Err(ConnectionProfileError::MissingName);
        }
        if !self.jdbc_url.trim().starts_with("jdbc:") {
            return Err(ConnectionProfileError::InvalidJdbcUrl);
        }
        if self.driver == DatabaseDriver::Custom {
            let path = self
                .custom_driver_path
                .as_ref()
                .ok_or(ConnectionProfileError::MissingCustomDriverPath)?;
            if !path.is_file() {
                return Err(ConnectionProfileError::CustomDriverNotFound(path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConnectionProfileError {
    #[error("connection name cannot be empty")]
    MissingName,
    #[error("connection URL must start with `jdbc:`")]
    InvalidJdbcUrl,
    #[error("a custom JDBC driver JAR is required")]
    MissingCustomDriverPath,
    #[error("custom JDBC driver JAR does not exist at `{}`", .0.display())]
    CustomDriverNotFound(PathBuf),
}

/// Versioned payload persisted by the database panel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectionRegistry {
    pub version: u32,
    pub connections: Vec<ConnectionProfile>,
}

impl ConnectionRegistry {
    pub fn add(&mut self, profile: ConnectionProfile) -> Result<(), ConnectionProfileError> {
        profile.validate()?;
        self.connections.push(profile);
        Ok(())
    }

    pub fn upsert(&mut self, profile: ConnectionProfile) -> Result<(), ConnectionProfileError> {
        profile.validate()?;
        match self
            .connections
            .iter_mut()
            .find(|connection| connection.id == profile.id)
        {
            Some(connection) => *connection = profile,
            None => self.connections.push(profile),
        }
        Ok(())
    }

    pub fn remove(&mut self, id: ConnectionId) -> Option<ConnectionProfile> {
        let index = self
            .connections
            .iter()
            .position(|connection| connection.id == id)?;
        Some(self.connections.remove(index))
    }
}

impl Default for ConnectionRegistry {
    fn default() -> Self {
        Self {
            version: CONNECTION_REGISTRY_VERSION,
            connections: Vec::new(),
        }
    }
}

/// A virtual `.sql` file stored for one connection in one Zed workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryConsole {
    pub id: ConsoleId,
    pub connection_id: ConnectionId,
    pub name: String,
    pub sql: String,
}

impl QueryConsole {
    pub fn new(connection_id: ConnectionId, name: impl Into<String>) -> Self {
        Self {
            id: ConsoleId::new(),
            connection_id,
            name: name.into(),
            sql: String::new(),
        }
    }
}

/// Versioned workspace-local collection of virtual SQL files.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConsoleRegistry {
    pub version: u32,
    pub consoles: Vec<QueryConsole>,
}

impl ConsoleRegistry {
    pub fn upsert(&mut self, console: QueryConsole) {
        match self
            .consoles
            .iter_mut()
            .find(|existing| existing.id == console.id)
        {
            Some(existing) => *existing = console,
            None => self.consoles.push(console),
        }
    }

    pub fn remove(&mut self, id: ConsoleId) -> Option<QueryConsole> {
        let index = self.consoles.iter().position(|console| console.id == id)?;
        Some(self.consoles.remove(index))
    }
}

impl Default for ConsoleRegistry {
    fn default() -> Self {
        Self {
            version: CONSOLE_REGISTRY_VERSION,
            consoles: Vec::new(),
        }
    }
}

/// Object kinds exposed by the lazy metadata tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseObjectKind {
    Catalog,
    Schema,
    Table,
    View,
    MaterializedView,
    Column,
    Index,
    PrimaryKey,
    ForeignKey,
    Constraint,
    Trigger,
    Sequence,
    Function,
    Procedure,
    DataType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_profile_with_driver_default() {
        let profile = ConnectionProfile::new("Local analytics", DatabaseDriver::ClickHouse);

        assert!(profile.read_only);
        assert_eq!(profile.jdbc_url, "jdbc:clickhouse://localhost:8123/default");
        assert_eq!(profile.validate(), Ok(()));
    }

    #[test]
    fn driver_storage_names_are_stable_and_accept_legacy_profiles() {
        let cases = [
            (DatabaseDriver::PostgreSql, "postgresql", "postgre_sql"),
            (DatabaseDriver::MySql, "mysql", "my_sql"),
            (DatabaseDriver::ClickHouse, "clickhouse", "click_house"),
            (DatabaseDriver::Sqlite, "sqlite", "sqlite"),
            (DatabaseDriver::Custom, "custom", "custom"),
        ];

        for (driver, protocol_name, legacy_name) in cases {
            assert_eq!(
                serde_json::to_string(&driver).unwrap(),
                format!("\"{protocol_name}\"")
            );
            assert_eq!(
                serde_json::from_str::<DatabaseDriver>(&format!("\"{legacy_name}\"")).unwrap(),
                driver
            );
        }
    }

    #[test]
    fn rejects_invalid_profiles() {
        let mut profile = ConnectionProfile::new("", DatabaseDriver::PostgreSql);
        assert_eq!(profile.validate(), Err(ConnectionProfileError::MissingName));

        profile.name = "Local".to_owned();
        profile.jdbc_url = "postgres://localhost/postgres".to_owned();
        assert_eq!(
            profile.validate(),
            Err(ConnectionProfileError::InvalidJdbcUrl)
        );

        profile.jdbc_url = "jdbc:custom://localhost/database".to_owned();
        assert_eq!(profile.validate(), Ok(()));
    }

    #[test]
    fn custom_profiles_require_an_existing_driver_jar() {
        let mut profile = ConnectionProfile::new("Custom", DatabaseDriver::Custom);
        assert_eq!(
            profile.validate(),
            Err(ConnectionProfileError::MissingCustomDriverPath)
        );

        let driver = tempfile::NamedTempFile::new().unwrap();
        profile.custom_driver_path = Some(driver.path().to_owned());
        profile.jdbc_url = "jdbc:custom://localhost/database".to_owned();
        assert_eq!(profile.validate(), Ok(()));
    }

    #[test]
    fn registry_round_trips_as_json() {
        let mut registry = ConnectionRegistry::default();
        registry
            .add(ConnectionProfile::new(
                "Local SQLite",
                DatabaseDriver::Sqlite,
            ))
            .unwrap();

        let json = serde_json::to_string(&registry).unwrap();
        let decoded = serde_json::from_str::<ConnectionRegistry>(&json).unwrap();

        assert_eq!(decoded, registry);
    }

    #[test]
    fn registry_upserts_and_removes_profiles() {
        let mut registry = ConnectionRegistry::default();
        let mut profile = ConnectionProfile::new("Local", DatabaseDriver::Sqlite);
        let id = profile.id;
        registry.upsert(profile.clone()).unwrap();

        profile.name = "Renamed".to_owned();
        registry.upsert(profile.clone()).unwrap();

        assert_eq!(registry.connections, vec![profile.clone()]);
        assert_eq!(registry.remove(id), Some(profile));
        assert!(registry.connections.is_empty());
    }

    #[test]
    fn console_registry_round_trips_and_upserts_virtual_files() {
        let connection_id = ConnectionId::new();
        let mut console = QueryConsole::new(connection_id, "console.sql");
        let console_id = console.id;
        let mut registry = ConsoleRegistry::default();
        registry.upsert(console.clone());

        console.sql = "select 1;".to_owned();
        registry.upsert(console.clone());

        let serialized = serde_json::to_string(&registry).unwrap();
        assert_eq!(
            serde_json::from_str::<ConsoleRegistry>(&serialized).unwrap(),
            registry
        );
        assert_eq!(registry.consoles, vec![console.clone()]);
        assert_eq!(registry.remove(console_id), Some(console));
    }
}
