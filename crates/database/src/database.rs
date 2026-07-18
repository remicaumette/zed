//! Domain types shared by the database viewer UI and its JDBC sidecar.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

mod sidecar;

pub use sidecar::{ConnectionTestResult, test_connection};

/// Version of the serialized connection registry.
pub const CONNECTION_REGISTRY_VERSION: u32 = 1;

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

/// Drivers supported by the first database viewer milestone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseDriver {
    PostgreSql,
    MySql,
    ClickHouse,
    Sqlite,
}

impl DatabaseDriver {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::PostgreSql => "PostgreSQL",
            Self::MySql => "MySQL",
            Self::ClickHouse => "ClickHouse",
            Self::Sqlite => "SQLite",
        }
    }

    pub fn default_jdbc_url(self) -> &'static str {
        match self {
            Self::PostgreSql => "jdbc:postgresql://localhost:5432/postgres",
            Self::MySql => "jdbc:mysql://localhost:3306/mysql",
            Self::ClickHouse => "jdbc:clickhouse://localhost:8123/default",
            Self::Sqlite => "jdbc:sqlite:database.sqlite",
        }
    }

    pub fn jdbc_url_prefix(self) -> &'static str {
        match self {
            Self::PostgreSql => "jdbc:postgresql:",
            Self::MySql => "jdbc:mysql:",
            Self::ClickHouse => "jdbc:clickhouse:",
            Self::Sqlite => "jdbc:sqlite:",
        }
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

/// Optional environment label used to make risky connections recognizable.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionEnvironment {
    #[default]
    Local,
    Development,
    Staging,
    Production,
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
    pub read_only: bool,
    #[serde(default)]
    pub environment: ConnectionEnvironment,
    #[serde(default)]
    pub scope: ConnectionScope,
}

impl ConnectionProfile {
    pub fn new(name: impl Into<String>, driver: DatabaseDriver) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            driver,
            jdbc_url: driver.default_jdbc_url().to_owned(),
            username: None,
            read_only: true,
            environment: ConnectionEnvironment::Local,
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
        if !self
            .jdbc_url
            .trim()
            .starts_with(self.driver.jdbc_url_prefix())
        {
            return Err(ConnectionProfileError::DriverUrlMismatch(self.driver));
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
    #[error("connection URL does not match the {0} JDBC driver")]
    DriverUrlMismatch(DatabaseDriver),
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
    fn creates_read_only_profile_with_driver_default() {
        let profile = ConnectionProfile::new("Local analytics", DatabaseDriver::ClickHouse);

        assert!(profile.read_only);
        assert_eq!(profile.jdbc_url, "jdbc:clickhouse://localhost:8123/default");
        assert_eq!(profile.validate(), Ok(()));
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

        profile.jdbc_url = "jdbc:mysql://localhost/mysql".to_owned();
        assert_eq!(
            profile.validate(),
            Err(ConnectionProfileError::DriverUrlMismatch(
                DatabaseDriver::PostgreSql
            ))
        );
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
}
