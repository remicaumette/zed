use crate::{ConnectionProfile, DatabaseDriver};
use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use http_client::HttpClient;
use sha2::{Digest as _, Sha256};
use std::{ffi::OsString, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JdbcDriverDownload {
    pub version: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

pub(crate) fn download_for_driver(driver: DatabaseDriver) -> Option<JdbcDriverDownload> {
    match driver {
        DatabaseDriver::PostgreSql => Some(JdbcDriverDownload {
            version: "42.7.11",
            file_name: "postgresql-42.7.11.jar",
            url: "https://repo1.maven.org/maven2/org/postgresql/postgresql/42.7.11/postgresql-42.7.11.jar",
            sha256: "1981b31d3993c58702783c1cddf10a34e48c1f413d70ff1cb6def0a143484647",
        }),
        DatabaseDriver::MySql => Some(JdbcDriverDownload {
            version: "9.7.0",
            file_name: "mysql-connector-j-9.7.0.jar",
            url: "https://repo1.maven.org/maven2/com/mysql/mysql-connector-j/9.7.0/mysql-connector-j-9.7.0.jar",
            sha256: "0353648eaa1c91e0f4020c959abf756bc866ffd583df22ae6b6f6e0cbd43eb44",
        }),
        DatabaseDriver::ClickHouse => Some(JdbcDriverDownload {
            version: "0.9.8",
            file_name: "clickhouse-jdbc-0.9.8-all.jar",
            url: "https://repo1.maven.org/maven2/com/clickhouse/clickhouse-jdbc/0.9.8/clickhouse-jdbc-0.9.8-all.jar",
            sha256: "9dbe9fd70388b67429be7942f53b7984a4b71fd0f2f727f17339f20e27ecf564",
        }),
        DatabaseDriver::Sqlite => Some(JdbcDriverDownload {
            version: "3.53.2.0",
            file_name: "sqlite-jdbc-3.53.2.0.jar",
            url: "https://repo1.maven.org/maven2/org/xerial/sqlite-jdbc/3.53.2.0/sqlite-jdbc-3.53.2.0.jar",
            sha256: "dc320e4102884c135ccc30c3c6fc3fb190b750e1586a100e3aba3be783cf33a9",
        }),
        DatabaseDriver::Custom => None,
    }
}

pub fn installed_jdbc_driver_path(driver: DatabaseDriver) -> Option<PathBuf> {
    let download = download_for_driver(driver)?;
    let path = jdbc_driver_directory().join(download.file_name);
    path.is_file().then_some(path)
}

pub fn resolve_jdbc_driver_path(profile: &ConnectionProfile) -> Result<PathBuf> {
    if profile.driver == DatabaseDriver::Custom {
        let path = profile
            .custom_driver_path
            .as_ref()
            .context("a custom JDBC driver JAR is required")?;
        if !path.is_file() {
            bail!(
                "custom JDBC driver JAR does not exist at {}",
                path.display()
            );
        }
        return Ok(path.clone());
    }

    installed_jdbc_driver_path(profile.driver).with_context(|| {
        let version = download_for_driver(profile.driver)
            .map(|download| download.version)
            .unwrap_or("unknown");
        format!(
            "{} JDBC driver {version} is not installed. Download it from the connection editor",
            profile.driver
        )
    })
}

pub async fn download_jdbc_driver(
    driver: DatabaseDriver,
    http_client: &dyn HttpClient,
) -> Result<PathBuf> {
    let download = download_for_driver(driver)
        .with_context(|| format!("{driver} uses a user-provided JDBC driver"))?;
    let directory = jdbc_driver_directory();
    async_fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("creating JDBC driver directory at {}", directory.display()))?;

    let destination = directory.join(download.file_name);
    let mut temporary_name = OsString::from(download.file_name);
    temporary_name.push(".part");
    let temporary = directory.join(temporary_name);

    let result = async {
        let mut response = http_client
            .get(download.url, Default::default(), true)
            .await
            .with_context(|| format!("downloading {} JDBC driver", driver))?;
        if !response.status().is_success() {
            bail!(
                "driver download failed with HTTP status {}",
                response.status()
            );
        }

        let mut output = async_fs::File::create(&temporary)
            .await
            .with_context(|| format!("creating {}", temporary.display()))?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = response
                .body_mut()
                .read(&mut buffer)
                .await
                .context("reading JDBC driver download")?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .await
                .context("writing JDBC driver download")?;
        }
        output
            .flush()
            .await
            .context("flushing JDBC driver download")?;
        drop(output);

        let actual_sha256 = format!("{:x}", digest.finalize());
        if actual_sha256 != download.sha256 {
            bail!(
                "JDBC driver checksum mismatch: expected {}, received {actual_sha256}",
                download.sha256
            );
        }

        async_fs::rename(&temporary, &destination)
            .await
            .with_context(|| format!("installing JDBC driver at {}", destination.display()))?;
        Ok(destination.clone())
    }
    .await;

    if result.is_err() {
        async_fs::remove_file(&temporary).await.ok();
    }
    result
}

fn jdbc_driver_directory() -> PathBuf {
    std::env::var_os("ZED_DATABASE_DRIVER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::data_dir().join("database-drivers"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_drivers_have_downloads_and_custom_does_not() {
        for driver in [
            DatabaseDriver::PostgreSql,
            DatabaseDriver::MySql,
            DatabaseDriver::ClickHouse,
            DatabaseDriver::Sqlite,
        ] {
            let download = download_for_driver(driver).unwrap();
            assert!(download.url.starts_with("https://repo1.maven.org/maven2/"));
            assert!(download.file_name.ends_with(".jar"));
            assert_eq!(download.sha256.len(), 64);
        }
        assert_eq!(download_for_driver(DatabaseDriver::Custom), None);
    }
}
