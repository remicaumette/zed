use crate::{ConnectionProfile, resolve_jdbc_driver_path};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

const PROTOCOL_VERSION: u32 = 4;
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTestResult {
    pub database_product: String,
    pub database_version: String,
    pub driver_name: String,
    pub driver_version: String,
    pub round_trip_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryColumn {
    pub label: String,
    pub type_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub columns: Vec<QueryColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected_rows: Option<u64>,
    pub truncated: bool,
    pub values_truncated: bool,
    pub has_more_rows: bool,
    pub elapsed_millis: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataDatabase {
    pub name: String,
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataTable {
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub table_type: String,
    pub identifier_quote: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataColumn {
    pub name: String,
    pub type_name: String,
    pub jdbc_type: i32,
    pub nullable: bool,
    pub auto_increment: bool,
    pub ordinal_position: u32,
    pub default_value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataIndex {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub referenced_catalog: Option<String>,
    pub referenced_schema: Option<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableMetadataDetails {
    pub columns: Vec<MetadataColumn>,
    pub indexes: Vec<MetadataIndex>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<MetadataForeignKey>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableMutationCell {
    pub column: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRowUpdate {
    pub key: Vec<TableMutationCell>,
    pub values: Vec<TableMutationCell>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableInsert {
    pub values: Vec<TableMutationCell>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRowDelete {
    pub key: Vec<TableMutationCell>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableChanges {
    pub updates: Vec<TableRowUpdate>,
    pub inserts: Vec<TableInsert>,
    pub deletes: Vec<TableRowDelete>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableMutationResult {
    pub updated: u64,
    pub inserted: u64,
    pub deleted: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestEnvelope<'a> {
    protocol_version: u32,
    request_id: Uuid,
    operation: &'static str,
    connection: ConnectionRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sql: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_rows: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    where_clause: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order_by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<&'a [TableMutationCell]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<&'a TableChanges>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionRequest<'a> {
    jdbc_url: &'a str,
    username: Option<&'a str>,
    password: Option<&'a str>,
    read_only: bool,
    timeout_seconds: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseEnvelope<T> {
    protocol_version: u32,
    request_id: Uuid,
    ok: bool,
    result: Option<T>,
    error: Option<SidecarError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarError {
    code: String,
    message: String,
    sql_state: Option<String>,
}

pub async fn test_connection(
    profile: &ConnectionProfile,
    password: Option<&str>,
) -> Result<ConnectionTestResult> {
    let request_id = Uuid::new_v4();
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            operation: "testConnection",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: None,
            offset: None,
            catalog: None,
            schema: None,
            table: None,
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        },
    )
    .await
}

pub async fn execute_query(
    profile: &ConnectionProfile,
    password: Option<&str>,
    sql: &str,
    max_rows: u32,
) -> Result<QueryResult> {
    if sql.trim().is_empty() {
        bail!("SQL cannot be empty");
    }
    if max_rows == 0 {
        bail!("query row limit must be greater than zero");
    }

    let request_id = Uuid::new_v4();
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            operation: "execute",
            connection: connection_request(profile, password),
            sql: Some(sql),
            max_rows: Some(max_rows),
            offset: None,
            catalog: None,
            schema: None,
            table: None,
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        },
    )
    .await
}

pub async fn list_databases(
    profile: &ConnectionProfile,
    password: Option<&str>,
) -> Result<Vec<MetadataDatabase>> {
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            operation: "listDatabases",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: None,
            offset: None,
            catalog: None,
            schema: None,
            table: None,
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        },
    )
    .await
}

pub async fn list_tables(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &MetadataDatabase,
) -> Result<Vec<MetadataTable>> {
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            operation: "listTables",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: None,
            offset: None,
            catalog: database.catalog.as_deref(),
            schema: database.schema.as_deref(),
            table: None,
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        },
    )
    .await
}

pub async fn describe_table(
    profile: &ConnectionProfile,
    password: Option<&str>,
    table: &MetadataTable,
) -> Result<TableMetadataDetails> {
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            operation: "describeTable",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: None,
            offset: None,
            catalog: table.catalog.as_deref(),
            schema: table.schema.as_deref(),
            table: Some(&table.name),
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        },
    )
    .await
}

pub async fn browse_table(
    profile: &ConnectionProfile,
    password: Option<&str>,
    table: &MetadataTable,
    where_clause: Option<&str>,
    order_by: Option<&str>,
    filters: &[TableMutationCell],
    offset: u32,
    max_rows: u32,
) -> Result<QueryResult> {
    if max_rows == 0 {
        bail!("table row limit must be greater than zero");
    }
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            operation: "browseTable",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: Some(max_rows),
            offset: Some(offset),
            catalog: table.catalog.as_deref(),
            schema: table.schema.as_deref(),
            table: Some(&table.name),
            where_clause: where_clause.filter(|clause| !clause.trim().is_empty()),
            order_by: order_by.filter(|order| !order.trim().is_empty()),
            filters: (!filters.is_empty()).then_some(filters),
            changes: None,
        },
    )
    .await
}

pub async fn apply_table_changes(
    profile: &ConnectionProfile,
    password: Option<&str>,
    table: &MetadataTable,
    changes: &TableChanges,
) -> Result<TableMutationResult> {
    if changes.updates.is_empty() && changes.inserts.is_empty() && changes.deletes.is_empty() {
        bail!("there are no table changes to save");
    }
    invoke(
        profile,
        RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4(),
            operation: "applyTableChanges",
            connection: connection_request(profile, password),
            sql: None,
            max_rows: None,
            offset: None,
            catalog: table.catalog.as_deref(),
            schema: table.schema.as_deref(),
            table: Some(&table.name),
            where_clause: None,
            order_by: None,
            filters: None,
            changes: Some(changes),
        },
    )
    .await
}

fn connection_request<'a>(
    profile: &'a ConnectionProfile,
    password: Option<&'a str>,
) -> ConnectionRequest<'a> {
    ConnectionRequest {
        jdbc_url: &profile.jdbc_url,
        username: profile.username.as_deref(),
        password,
        read_only: profile.read_only,
        timeout_seconds: 10,
    }
}

async fn invoke<T>(profile: &ConnectionProfile, request: RequestEnvelope<'_>) -> Result<T>
where
    T: DeserializeOwned,
{
    profile.validate()?;
    let jar_path = sidecar_jar_path();
    if !jar_path.is_file() {
        bail!(
            "JDBC sidecar is not built at {}. Run `script/build-database-sidecar`",
            jar_path.display()
        );
    }
    let driver_path = resolve_jdbc_driver_path(profile)?;
    let classpath = std::env::join_paths([jar_path.as_os_str(), driver_path.as_os_str()])
        .context("building the JDBC sidecar classpath")?;

    let request_id = request.request_id;
    let payload = serde_json::to_vec(&request).context("serializing JDBC request")?;
    if payload.len() > MAX_FRAME_SIZE {
        bail!("JDBC request exceeds the maximum frame size");
    }

    let java_binary = std::env::var_os("ZED_DATABASE_JAVA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("java"));
    let mut child = async_process::Command::new(&java_binary)
        .arg("-cp")
        .arg(classpath)
        .arg("dev.zed.database.sidecar.Main")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "starting Java from `{}`; set ZED_DATABASE_JAVA to override it",
                java_binary.display()
            )
        })?;

    let mut stdin = child
        .stdin
        .take()
        .context("opening JDBC sidecar standard input")?;
    stdin
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .context("writing JDBC request frame length")?;
    stdin
        .write_all(&payload)
        .await
        .context("writing JDBC request frame")?;
    stdin.flush().await.context("flushing JDBC request")?;
    drop(stdin);

    let mut stdout = child
        .stdout
        .take()
        .context("opening JDBC sidecar standard output")?;
    let mut frame_length = [0_u8; 4];
    stdout
        .read_exact(&mut frame_length)
        .await
        .context("reading JDBC response frame length")?;
    let frame_length = u32::from_be_bytes(frame_length) as usize;
    if frame_length > MAX_FRAME_SIZE {
        bail!("JDBC response exceeds the maximum frame size");
    }
    let mut response_payload = vec![0; frame_length];
    stdout
        .read_exact(&mut response_payload)
        .await
        .context("reading JDBC response frame")?;

    let mut stderr = child
        .stderr
        .take()
        .context("opening JDBC sidecar standard error")?;
    let mut stderr_output = String::new();
    stderr
        .read_to_string(&mut stderr_output)
        .await
        .context("reading JDBC sidecar diagnostics")?;
    let status = child.status().await.context("waiting for JDBC sidecar")?;
    if !status.success() {
        bail!(
            "JDBC sidecar exited with {status}: {}",
            stderr_output.trim()
        );
    }

    let response = serde_json::from_slice::<ResponseEnvelope<T>>(&response_payload)
        .context("decoding JDBC response")?;
    if response.protocol_version != PROTOCOL_VERSION {
        bail!(
            "JDBC sidecar protocol mismatch: expected {}, received {}",
            PROTOCOL_VERSION,
            response.protocol_version
        );
    }
    if response.request_id != request_id {
        bail!("JDBC sidecar returned a response for an unknown request");
    }
    if response.ok {
        return response
            .result
            .context("JDBC sidecar returned an empty successful response");
    }

    let error = response
        .error
        .ok_or_else(|| anyhow!("JDBC sidecar returned an unspecified error"))?;
    let sql_state = error
        .sql_state
        .map(|state| format!(" (SQLSTATE {state})"))
        .unwrap_or_default();
    bail!("{}{}: {}", error.code, sql_state, error.message)
}

fn sidecar_jar_path() -> PathBuf {
    std::env::var_os("ZED_DATABASE_SIDECAR_JAR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/database-sidecar/target/database-sidecar.jar")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DatabaseDriver;

    #[test]
    fn request_never_serializes_absent_credentials() {
        let profile = ConnectionProfile::new("Local", DatabaseDriver::Sqlite);
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::nil(),
            operation: "testConnection",
            connection: ConnectionRequest {
                jdbc_url: &profile.jdbc_url,
                username: None,
                password: None,
                read_only: profile.read_only,
                timeout_seconds: 10,
            },
            sql: None,
            max_rows: None,
            offset: None,
            catalog: None,
            schema: None,
            table: None,
            where_clause: None,
            order_by: None,
            filters: None,
            changes: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("\"driver\""));
        assert!(json.contains("\"password\":null"));
        assert!(!json.contains("Local"));
    }

    #[test]
    #[ignore = "requires Java and a built JDBC sidecar"]
    fn connects_to_sqlite_end_to_end() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("smoke-test.sqlite");
        let mut profile = ConnectionProfile::new("Smoke test", DatabaseDriver::Sqlite);
        profile.jdbc_url = format!("jdbc:sqlite:{}", database_path.display());

        let result = smol::block_on(test_connection(&profile, None)).unwrap();

        assert_eq!(result.database_product, "SQLite");
        assert!(database_path.is_file());
    }

    #[test]
    #[ignore = "requires Java and a built JDBC sidecar"]
    fn executes_sqlite_query_end_to_end() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("query-smoke-test.sqlite");
        let mut profile = ConnectionProfile::new("Query smoke test", DatabaseDriver::Sqlite);
        profile.jdbc_url = format!("jdbc:sqlite:{}", database_path.display());

        let result = smol::block_on(execute_query(
            &profile,
            None,
            "select 42 as answer, null as missing",
            100,
        ))
        .unwrap();

        assert_eq!(result.columns[0].label, "answer");
        assert_eq!(result.rows, vec![vec![Some("42".into()), None]]);
        assert_eq!(result.affected_rows, None);
        assert!(!result.truncated);
        assert!(!result.values_truncated);

        let result = smol::block_on(execute_query(
            &profile,
            None,
            "with recursive counter(value) as (values(1) union all select value + 1 from counter where value < 20) select value from counter",
            10,
        ))
        .unwrap();
        assert_eq!(result.rows.len(), 10);
        assert!(result.truncated);
        assert!(!result.values_truncated);

        let result = smol::block_on(execute_query(
            &profile,
            None,
            "select printf('%05000d', 1) as large_value",
            10,
        ))
        .unwrap();
        assert_eq!(result.rows[0][0].as_ref().unwrap().len(), 4096);
        assert!(result.truncated);
        assert!(result.values_truncated);
    }

    #[test]
    #[ignore = "requires Java and a built JDBC sidecar"]
    fn browses_sqlite_metadata_and_table_data_end_to_end() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("metadata-smoke-test.sqlite");
        let mut profile = ConnectionProfile::new("Metadata smoke test", DatabaseDriver::Sqlite);
        profile.jdbc_url = format!("jdbc:sqlite:{}", database_path.display());
        profile.read_only = false;

        smol::block_on(execute_query(
            &profile,
            None,
            "create table categories(id integer primary key, name text not null)",
            100,
        ))
        .unwrap();
        smol::block_on(execute_query(
            &profile,
            None,
            "create table widgets(id integer primary key, name text not null, score integer, category_id integer references categories(id))",
            100,
        ))
        .unwrap();
        smol::block_on(execute_query(
            &profile,
            None,
            "create unique index widgets_name_idx on widgets(name)",
            100,
        ))
        .unwrap();
        smol::block_on(execute_query(
            &profile,
            None,
            "insert into categories(name) values ('primary'), ('O''Reilly')",
            100,
        ))
        .unwrap();
        smol::block_on(execute_query(
            &profile,
            None,
            "insert into widgets(name, score, category_id) values ('alpha', 2, 1), ('beta', 1, 1)",
            100,
        ))
        .unwrap();

        let databases = smol::block_on(list_databases(&profile, None)).unwrap();
        assert_eq!(databases.len(), 1);
        let tables = smol::block_on(list_tables(&profile, None, &databases[0])).unwrap();
        let widgets = tables.iter().find(|table| table.name == "widgets").unwrap();
        let categories = tables
            .iter()
            .find(|table| table.name == "categories")
            .unwrap();
        let related = smol::block_on(browse_table(
            &profile,
            None,
            categories,
            None,
            None,
            &[TableMutationCell {
                column: "name".into(),
                value: Some("O'Reilly".into()),
            }],
            0,
            100,
        ))
        .unwrap();
        assert_eq!(related.rows.len(), 1);
        assert_eq!(related.rows[0][1], Some("O'Reilly".into()));
        let details = smol::block_on(describe_table(&profile, None, widgets)).unwrap();
        assert_eq!(
            details
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name", "score", "category_id"]
        );
        assert_eq!(details.primary_key, vec!["id"]);
        assert_eq!(details.foreign_keys.len(), 1);
        assert_eq!(details.foreign_keys[0].columns, vec!["category_id"]);
        assert_eq!(details.foreign_keys[0].referenced_table, "categories");
        assert_eq!(details.foreign_keys[0].referenced_columns, vec!["id"]);
        assert!(
            details
                .indexes
                .iter()
                .any(|index| index.name == "widgets_name_idx" && index.unique)
        );

        let result = smol::block_on(browse_table(
            &profile,
            None,
            widgets,
            Some("score >= 1"),
            Some("score asc"),
            &[],
            0,
            100,
        ))
        .unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][1], Some("beta".into()));

        let first_page = smol::block_on(browse_table(
            &profile,
            None,
            widgets,
            None,
            Some("name asc"),
            &[],
            0,
            1,
        ))
        .unwrap();
        assert_eq!(first_page.rows[0][1], Some("alpha".into()));
        assert!(first_page.has_more_rows);
        let second_page = smol::block_on(browse_table(
            &profile,
            None,
            widgets,
            None,
            Some("name asc"),
            &[],
            1,
            1,
        ))
        .unwrap();
        assert_eq!(second_page.rows[0][1], Some("beta".into()));
        assert!(!second_page.has_more_rows);

        let beta_id = result.rows[0][0].clone().unwrap();
        let alpha_id = result.rows[1][0].clone().unwrap();
        let changes = TableChanges {
            updates: vec![TableRowUpdate {
                key: vec![TableMutationCell {
                    column: "id".into(),
                    value: Some(beta_id),
                }],
                values: vec![TableMutationCell {
                    column: "score".into(),
                    value: Some("5".into()),
                }],
            }],
            inserts: vec![TableInsert {
                values: vec![
                    TableMutationCell {
                        column: "name".into(),
                        value: Some("gamma".into()),
                    },
                    TableMutationCell {
                        column: "score".into(),
                        value: Some("3".into()),
                    },
                ],
            }],
            deletes: vec![TableRowDelete {
                key: vec![TableMutationCell {
                    column: "id".into(),
                    value: Some(alpha_id),
                }],
            }],
        };
        let mutation =
            smol::block_on(apply_table_changes(&profile, None, widgets, &changes)).unwrap();
        assert_eq!(
            mutation,
            TableMutationResult {
                updated: 1,
                inserted: 1,
                deleted: 1,
            }
        );
        let result = smol::block_on(browse_table(
            &profile,
            None,
            widgets,
            None,
            Some("name asc"),
            &[],
            0,
            100,
        ))
        .unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][1], Some("beta".into()));
        assert_eq!(result.rows[0][2], Some("5".into()));
        assert_eq!(result.rows[1][1], Some("gamma".into()));

        smol::block_on(execute_query(
            &profile,
            None,
            "create table logs(message text)",
            100,
        ))
        .unwrap();
        let tables = smol::block_on(list_tables(&profile, None, &databases[0])).unwrap();
        let logs = tables.iter().find(|table| table.name == "logs").unwrap();
        let error = smol::block_on(apply_table_changes(
            &profile,
            None,
            logs,
            &TableChanges {
                inserts: vec![TableInsert {
                    values: vec![TableMutationCell {
                        column: "message".into(),
                        value: Some("unsafe".into()),
                    }],
                }],
                ..TableChanges::default()
            },
        ))
        .unwrap_err();
        assert!(error.to_string().contains("primary key"));
    }

    #[test]
    #[ignore = "requires Java and a built JDBC sidecar"]
    fn resolves_all_managed_jdbc_drivers() {
        let cases = [
            (
                DatabaseDriver::PostgreSql,
                "jdbc:postgresql://127.0.0.1:1/postgres?connectTimeout=1",
            ),
            (
                DatabaseDriver::MySql,
                "jdbc:mysql://127.0.0.1:1/mysql?connectTimeout=1000",
            ),
            (
                DatabaseDriver::ClickHouse,
                "jdbc:clickhouse://127.0.0.1:1/default?connection_timeout=1000",
            ),
        ];

        for (driver, jdbc_url) in cases {
            let mut profile = ConnectionProfile::new("Driver test", driver);
            profile.jdbc_url = jdbc_url.to_owned();
            let error = smol::block_on(test_connection(&profile, None)).unwrap_err();
            let message = error.to_string();
            assert!(
                !message.contains("No suitable driver"),
                "{driver} was not resolved: {message}"
            );
        }
    }
}
