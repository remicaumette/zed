---
title: Database Viewer Project
description: "Architecture, roadmap, and test plan for Zed's database viewer."
---

# Database Viewer Project

This page tracks the database viewer developed in this fork. The long-term goal
is an integrated database workspace comparable to the database tools in
IntelliJ IDEA and DataGrip.

The first supported databases are PostgreSQL, MySQL, ClickHouse, and SQLite.

## Current status

Last updated: July 18, 2026.

The project now spans milestones 0 and 1. Connection profiles can be edited and
tested against a real database through the JDBC sidecar.

- [x] Add versioned connection profile types.
- [x] Validate JDBC URLs without storing secrets in the profile.
- [x] Register a native Database panel in Zed's dock.
- [x] List, create, edit, remove, and persist connection profiles.
- [x] Configure JDBC URL, username, password, environment, and read-only mode.
- [x] Store passwords through Zed's credential provider instead of profile data.
- [x] Start the JDBC sidecar and validate its protocol version.
- [x] Test a real connection from the editor and display driver metadata.
- [x] Exercise the complete protocol against a temporary SQLite database.
- [ ] Run contract tests against PostgreSQL, MySQL, and ClickHouse containers.
- [ ] Package the sidecar as part of release builds.

## Architecture

The UI remains native Rust and GPUI. JDBC runs in a separate Java process:

```text
Database panel and query views (Rust/GPUI)
                |
Connection and metadata services (Rust)
                |
Framed, versioned protocol over stdin/stdout
                |
JDBC sidecar (Java)
                |
PostgreSQL | MySQL | ClickHouse | SQLite JDBC drivers
```

Running JDBC out of process keeps the JVM out of Zed's address space. A driver
or JVM crash cannot corrupt the editor process, and Java dependencies stay out
of Cargo's dependency graph.

Protocol version 1 uses length-prefixed JSON envelopes. Each request has an ID
and an explicit protocol version. Standard output is reserved for protocol
frames; diagnostics go to standard error. An incompatible version is rejected
with an actionable error. Deadlines, cancellation, and a persistent sidecar
process are still planned before query execution is added.

### Crate boundaries

- `database` owns profiles, object kinds, protocol-facing domain types, and
  service interfaces. It does not depend on GPUI or Java.
- `database_ui` owns the dock panel, connection editor, object tree, query
  views, and presentation state.
- The Java module owns JDBC driver loading and connection tests. It will also
  own connection pools, metadata adapters, statement execution, and result
  streaming.
- Zed's main crate only initializes the subsystem and adds its panel.

Zed already has a virtualized table component with dynamic, resizable, and
pinned columns. Query results should extend that component instead of creating
a second data grid.

## Connection model

A saved profile contains a stable ID, display name, JDBC URL, username, scope,
environment label, and read-only preference. Passwords and access tokens are
never serialized with the profile.

Credentials use Zed's credential provider. They are sent to the sidecar only
when opening a connection and are never returned in errors or logs. Development
builds use Zed's development credential store by default; set
`ZED_DEVELOPMENT_USE_KEYCHAIN=1` before launching Zed to exercise the operating
system keychain.

Connections default to read-only. Production profiles will receive a visible
warning treatment before any write-capable mode is enabled.

## Driver strategy

| Database   | JDBC driver                                                                             | Pinned version | First metadata adapter |
| ---------- | --------------------------------------------------------------------------------------- | -------------- | ---------------------- |
| PostgreSQL | [PostgreSQL JDBC](https://central.sonatype.com/artifact/org.postgresql/postgresql)      | 42.7.11        | PostgreSQL             |
| MySQL      | [MySQL Connector/J](https://central.sonatype.com/artifact/com.mysql/mysql-connector-j)  | 9.7.0          | MySQL                  |
| ClickHouse | [ClickHouse JDBC](https://central.sonatype.com/artifact/com.clickhouse/clickhouse-jdbc) | 0.9.8          | ClickHouse             |
| SQLite     | [Xerial SQLite JDBC](https://central.sonatype.com/artifact/org.xerial/sqlite-jdbc)      | 3.53.2.0       | SQLite                 |

`DatabaseMetaData` supplies the common baseline. Small dialect adapters will
fill gaps and normalize database-specific behavior. This preserves JDBC's
driver reuse without pretending every database exposes identical metadata.

## Object explorer

The explorer will load children lazily and cache metadata per connection. The
target object model includes:

- catalogs and schemas;
- tables, views, and materialized views;
- columns and data types;
- primary keys, foreign keys, indexes, and constraints;
- sequences, functions, procedures, and triggers where supported.

Refresh invalidates only the selected subtree. Loading one schema must not scan
an entire server, which is important for large PostgreSQL and ClickHouse
installations.

## Roadmap

### Milestone 0: connection list

- Native dock panel and actions.
- Versioned persistence for non-secret profile settings.
- Connection creation, editing, duplication, and removal.
- Environment labels and read-only defaults.

Exit condition: profiles for all four drivers survive a Zed restart and can be
edited without touching a configuration file.

### Milestone 1: JDBC runtime

- Reproducible sidecar build and packaging.
- Java runtime discovery with clear setup errors.
- Driver allowlist and deterministic driver versions.
- Handshake, health check, timeouts, cancellation, and crash recovery.
- Test Connection action with sanitized diagnostics.

Exit condition: each supported database passes a real connection test and a
sidecar restart does not require restarting Zed.

### Milestone 2: object explorer

- Lazy catalogs, schemas, tables, views, columns, keys, and indexes.
- Refresh at connection and subtree level.
- Search and filtering.
- Copy qualified name and generate basic SQL actions.

Exit condition: the four drivers pass the same metadata contract tests against
containerized databases and a temporary SQLite file.

### Milestone 3: query console

- SQL editor associated with a connection and schema.
- Execute selection or statement with cancellation and timeout.
- Stream bounded result batches into Zed's virtualized table.
- Display affected rows, duration, warnings, and sanitized errors.

Exit condition: large results do not block the UI or grow memory without a
configured bound.

### Milestone 4: DataGrip-style workflows

- Multiple result tabs and query history.
- DDL preview, data export, and explain plans.
- Safe data editing with explicit primary-key requirements.
- Schema diff and richer database-specific object support.

This milestone will be split into smaller proposals before implementation.

## Quality gates

Build the JDBC sidecar first. The script uses an installed Maven binary when
available, otherwise it downloads Maven into Zed's ignored `target` directory.

```sh
script/build-database-sidecar
```

Run the smallest relevant checks during development:

```sh
cargo test -p database
cargo test -p database sidecar::tests::connects_to_sqlite_end_to_end -- --ignored --exact
cargo check -p database_ui
cargo check -p zed
```

Format and verify documentation changes:

```sh
cd docs
npx prettier --write src/
npx prettier --check src/
```

Before a milestone is complete, test sidecar error paths as well as the happy
path: missing Java, missing driver, invalid credentials, timeout, cancellation,
protocol mismatch, process crash, and network loss.

## Manual test checklist

For the current connection-editor and JDBC slice:

1. Run `script/build-database-sidecar`.
2. Build and launch the development version of Zed.
3. Open the Database panel using its database icon in the right dock.
4. Select SQLite and keep the generated JDBC URL, or point it at a disposable
   file.
5. Change the name, environment, and read-only setting, then select **Test
   Connection**.
6. Confirm that the success message contains SQLite and JDBC driver versions.
7. Save the connection, close and reopen Zed, and confirm the profile remains.
8. Edit the saved profile, enter a password if the target database needs one,
   save it, then reopen the editor. The password field must remain visually
   empty while **Test Connection** continues to use the stored secret.
9. Remove the profile, restart Zed, and confirm it stays removed.

Then repeat the test with available PostgreSQL, MySQL, and ClickHouse instances.
Use driver-specific JDBC URLs such as:

```text
jdbc:postgresql://localhost:5432/postgres
jdbc:mysql://localhost:3306/mysql
jdbc:clickhouse://localhost:8123/default
jdbc:sqlite:database.sqlite
```

The sidecar currently starts once per connection test. Persistent process
management, cancellation, and metadata browsing are the next runtime slice.

## Decisions

### ADR-001: fork instead of extension

The current extension API cannot add the required native dock, result grid,
credential integration, and long-lived process lifecycle. The first version is
therefore implemented in a fork. Re-evaluate upstreaming once crate boundaries
and extension requirements are clear.

### ADR-002: JDBC sidecar instead of native drivers

Use one mature driver ecosystem for the four initial databases. Keep the JVM
isolated behind a process boundary and a narrow protocol.

### ADR-003: generic metadata plus dialect adapters

Use JDBC metadata for portable concepts and explicit adapters for correctness.
Avoid scattering driver-specific conditions throughout the UI.

### ADR-004: secrets are not profile data

Persist non-secret settings through Zed's storage. Persist secrets only through
the credential provider. Redact connection strings and driver errors before
showing or logging them.
