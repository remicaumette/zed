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

The project is in milestone 0. The first vertical slice is under development.

- [x] Add versioned connection profile types.
- [x] Validate JDBC URLs without storing secrets in the profile.
- [x] Register a native Database panel in Zed's dock.
- [x] List, create, remove, and persist local connection drafts.
- [ ] Replace quick-add drafts with a complete connection editor.
- [ ] Start the JDBC sidecar and negotiate its protocol version.
- [ ] Test a real connection from the editor.

The quick-add controls are temporary. They make the list and persistence
testable before the connection editor and JDBC runtime are available.

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

The sidecar protocol will use length-prefixed Protobuf messages. Each request
will have an ID, deadline, and cancellation path. Standard output is reserved
for protocol frames; logs go to standard error. The handshake will reject an
incompatible protocol version with an actionable error.

### Crate boundaries

- `database` owns profiles, object kinds, protocol-facing domain types, and
  service interfaces. It does not depend on GPUI or Java.
- `database_ui` owns the dock panel, connection editor, object tree, query
  views, and presentation state.
- The future Java module owns JDBC driver loading, connection pools, metadata
  adapters, statement execution, and result streaming.
- Zed's main crate only initializes the subsystem and adds its panel.

Zed already has a virtualized table component with dynamic, resizable, and
pinned columns. Query results should extend that component instead of creating
a second data grid.

## Connection model

A saved profile contains a stable ID, display name, JDBC URL, username, scope,
environment label, and read-only preference. Passwords and access tokens are
never serialized with the profile.

Credentials will use Zed's credential provider. They are sent to the sidecar
only when opening a connection and are never returned in errors or logs.

Connections default to read-only. Production profiles will receive a visible
warning treatment before any write-capable mode is enabled.

## Driver strategy

| Database   | JDBC driver        | First metadata adapter |
| ---------- | ------------------ | ---------------------- |
| PostgreSQL | PostgreSQL JDBC    | PostgreSQL             |
| MySQL      | MySQL Connector/J  | MySQL                  |
| ClickHouse | ClickHouse JDBC    | ClickHouse             |
| SQLite     | Xerial SQLite JDBC | SQLite                 |

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

Run the smallest relevant checks during development:

```sh
cargo test -p database
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

For the current connection-list slice:

1. Build and launch the development version of Zed.
2. Open the Database panel using its database icon in the right dock.
3. Add one draft for each supported driver.
4. Close and reopen Zed, then confirm the four profiles are still listed.
5. Remove a profile, restart Zed, and confirm it stays removed.

Do not enter real credentials yet. The current slice does not expose a
credential editor or connect to a database.

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
