use crate::{ConnectionProfile, resolve_jdbc_driver_path};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTestResult {
    pub database_product: String,
    pub database_version: String,
    pub driver_name: String,
    pub driver_version: String,
    pub round_trip_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestEnvelope<'a> {
    protocol_version: u32,
    request_id: Uuid,
    operation: &'static str,
    connection: ConnectionRequest<'a>,
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
struct ResponseEnvelope {
    protocol_version: u32,
    request_id: Uuid,
    ok: bool,
    result: Option<ConnectionTestResult>,
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

    let request_id = Uuid::new_v4();
    let request = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id,
        operation: "testConnection",
        connection: ConnectionRequest {
            jdbc_url: &profile.jdbc_url,
            username: profile.username.as_deref(),
            password,
            read_only: profile.read_only,
            timeout_seconds: 10,
        },
    };

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

    let response = serde_json::from_slice::<ResponseEnvelope>(&response_payload)
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
