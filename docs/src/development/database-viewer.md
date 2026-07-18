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

The project now spans milestones 0 through 4. Connection profiles can be edited
and tested against a real database. Each saved connection can own multiple
workspace-local SQL consoles, lazily browse JDBC metadata, and open table data
in a native Zed view with staged row editing.

- [x] Add versioned connection profile types.
- [x] Validate JDBC URLs without storing secrets in the profile.
- [x] Register a native Database panel in Zed's dock.
- [x] List, create, edit, remove, and persist connection profiles.
- [x] Configure JDBC URL, username, password, and read-only mode.
- [x] Store passwords through Zed's credential provider instead of profile data.
- [x] Start the JDBC sidecar and validate its protocol version.
- [x] Test a real connection from the editor and display driver metadata.
- [x] Download missing built-in drivers on demand with SHA-256 verification.
- [x] Accept a user-provided JAR for custom JDBC connections.
- [x] Select the database driver inside a single New Connection dialog.
- [x] Create multiple persistent `.sql` consoles in a dedicated Consoles group.
- [x] Edit a console with Zed's native editor and SQL language support when installed.
- [x] Parse and execute the selection, the current statement, or the whole document.
- [x] Display one result tab per executed statement in a dedicated bottom panel.
- [x] Display bounded rows, affected-row counts, durations, truncation, and errors.
- [x] Lazily list databases, tables, views, columns, and indexes through JDBC metadata.
- [x] Open a table data tab with optional `WHERE` and `ORDER BY` fragments.
- [x] Submit `WHERE` and `ORDER BY` with Enter and refresh explicitly.
- [x] Cycle ascending, descending, and unsorted order by selecting a result column.
- [x] Display row and column borders so editable cells are clearly delimited.
- [x] Edit cells inline, add rows, and stage row deletions for tables with a primary key.
- [x] Pick calendar dates for JDBC date and timestamp cells while preserving timestamp times.
- [x] Select rows and expose NULL, delete, and foreign-key navigation context actions.
- [x] Page through table data in bounded 100-row JDBC result windows.
- [x] Confirm before discarding staged changes during result navigation.
- [x] Save all staged table changes in one transaction using prepared statements.
- [x] Keep connections and tables without a JDBC-reported primary key read-only.
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
Managed or custom JDBC driver JAR
                |
Framed, versioned protocol over stdin/stdout
                |
JDBC sidecar (Java)
                |
PostgreSQL | MySQL | ClickHouse | SQLite JDBC drivers
```

Running JDBC out of process keeps the JVM out of Zed's address space. A driver
or JVM crash cannot corrupt the editor process, and Java dependencies stay out
of Cargo's dependency graph. The sidecar itself contains no database-specific
driver: Zed adds the selected JAR to the Java classpath and `DriverManager`
discovers it from the JDBC URL.

Protocol version 4 uses length-prefixed JSON envelopes. Each request has an ID
and an explicit protocol version. Standard output is reserved for protocol
frames; diagnostics go to standard error. An incompatible version is rejected
with an actionable error. Table browsing returns pages of at most 100 rows,
truncates oversized cell/result text, and caps protocol frames at 16 MiB.
Deadlines beyond JDBC's optional timeout, cancellation, result streaming, and a
persistent sidecar process are still planned.

Table writes are staged in the Rust UI and sent as a single mutation request.
The sidecar independently verifies that the connection is writable, discovers
the table and primary key through JDBC metadata, validates every identifier,
and executes parameterized `INSERT`, `UPDATE`, and `DELETE` statements in one
transaction. A failed statement rolls the complete change set back.
Foreign-key navigation uses structured, metadata-validated predicates and JDBC
prepared statements. The readable `WHERE` shown in the new tab is not used as
raw SQL until the user edits it explicitly.

### Crate boundaries

- `database` owns profiles, object kinds, protocol-facing domain types, and
  service interfaces. It also owns the dialect-neutral statement splitter. It
  does not depend on GPUI or Java.
- `database_ui` owns the dock panel, connection editor, object tree, query
  consoles, and the bottom results panel.
- The Rust driver manager owns verified downloads, installed-driver state, and
  custom JAR paths.
- The Java module owns generic JDBC connection tests and bounded statement
  execution. It will also own connection pools and result streaming.
- Zed's main crate only initializes the subsystem and adds its panel.

Zed already has a virtualized table component with dynamic, resizable, and
pinned columns. Query results should extend that component instead of creating
a second data grid.

## Connection model

A saved profile contains a stable ID, display name, JDBC URL, username, scope,
and read-only preference. Passwords and access tokens are never serialized with
the profile.

Credentials use Zed's credential provider. They are sent to the sidecar only
when opening a connection and are never returned in errors or logs. Development
builds use Zed's development credential store by default; set
`ZED_DEVELOPMENT_USE_KEYCHAIN=1` before launching Zed to exercise the operating
system keychain.

Connections default to read-only. The JDBC flag is applied during connection
tests and query execution.

## Driver strategy

| Choice      | JDBC driver                                                                             | Version / source | First metadata adapter |
| ----------- | --------------------------------------------------------------------------------------- | ---------------- | ---------------------- |
| PostgreSQL  | [PostgreSQL JDBC](https://central.sonatype.com/artifact/org.postgresql/postgresql)      | 42.7.11          | PostgreSQL             |
| MySQL       | [MySQL Connector/J](https://central.sonatype.com/artifact/com.mysql/mysql-connector-j)  | 9.7.0            | MySQL                  |
| ClickHouse  | [ClickHouse JDBC](https://central.sonatype.com/artifact/com.clickhouse/clickhouse-jdbc) | 0.9.8 `all` JAR  | ClickHouse             |
| SQLite      | [Xerial SQLite JDBC](https://central.sonatype.com/artifact/org.xerial/sqlite-jdbc)      | 3.53.2.0         | SQLite                 |
| Custom JDBC | User-provided JAR                                                                       | User-managed     | Generic JDBC           |

The four predefined drivers are not shipped inside the sidecar. The connection
editor shows whether the selected version is installed and offers a download
when it is missing. Downloads come from Maven Central and are accepted only
when their SHA-256 digest matches the pinned value. A custom JDBC connection
instead stores the selected local JAR path in its non-secret profile.

`DatabaseMetaData` supplies the common baseline. Small dialect adapters will
fill gaps and normalize database-specific behavior. This preserves JDBC's
driver reuse without pretending every database exposes identical metadata.

## Object explorer

The explorer loads databases, tables, columns, indexes, and foreign keys lazily and caches
metadata for the current connection. Discovery stays generic: the sidecar uses
`DatabaseMetaData` instead of branching on the selected driver. Schemas are
currently flattened into qualified table labels below each database.

The target object model includes:

- catalogs and schemas;
- tables, views, and materialized views;
- columns and data types;
- [x] primary keys, foreign keys, and indexes;
- constraints beyond primary and foreign keys;
- sequences, functions, procedures, and triggers where supported.

Refresh invalidates only the selected subtree. Loading one schema must not scan
an entire server, which is important for large PostgreSQL and ClickHouse
installations.

## Roadmap

### Milestone 0: connection list

- Native dock panel and actions.
- Versioned persistence for non-secret profile settings.
- Connection creation, editing, duplication, and removal.
- Clearly labelled read-only mode, enabled by default.

Exit condition: profiles for all four drivers survive a Zed restart and can be
edited without touching a configuration file.

### Milestone 1: JDBC runtime

- Reproducible sidecar build and packaging.
- Java runtime discovery with clear setup errors.
- Deterministic managed driver versions and custom JAR support.
- Handshake, health check, timeouts, cancellation, and crash recovery.
- Test Connection action with sanitized diagnostics.

Exit condition: each supported database passes a real connection test and a
sidecar restart does not require restarting Zed.

### Milestone 2: object explorer

- [x] Lazy databases, tables, views, columns, indexes, primary keys, and foreign keys.
- [x] Refresh at database-root level.
- Add explicit schema nodes, remaining constraints, functions, and procedures.
- Add refresh at every subtree level.
- Search and filtering.
- Copy qualified name and generate basic SQL actions.

Exit condition: the four drivers pass the same metadata contract tests against
containerized databases and a temporary SQLite file.

### Milestone 3: query console

- [x] Multiple native SQL editor tabs associated with a saved connection.
- [x] Workspace-local persistence for virtual `.sql` console documents.
- [x] Lexically split statements across comments and quoted values without
      coupling the UI to a specific JDBC driver.
- [x] Execute the selection, current statement, or complete document.
- [x] Show one bounded result per statement in a dedicated bottom panel.
- Add cancellation, configurable timeouts, and a shared connection session.
- Stream bounded result batches into Zed's virtualized table.
- Display JDBC warnings and improve sanitized diagnostics.

Exit condition: large results do not block the UI or grow memory without a
configured bound.

### Milestone 4: DataGrip-style workflows

- Persistent result tabs and query history across executions.
- [x] Editable table data with safe primary-key requirements.
- [x] Staged inserts, updates, and deletions with atomic save and discard.
- [x] Calendar picker for JDBC date and timestamp values.
- Improve JDBC value editors for binary, JSON, and database-specific types.
- Add optimistic concurrency checks and a generated SQL preview.
- DDL preview, data export, and explain plans.
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
cargo test -p database sidecar::tests::executes_sqlite_query_end_to_end -- --ignored --exact
cargo test -p database sidecar::tests::browses_sqlite_metadata_and_table_data_end_to_end -- --ignored --exact
cargo test -p database sidecar::tests::resolves_all_managed_jdbc_drivers -- --ignored --exact
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

For the current connection-editor, SQL console, and JDBC slice:

1. Run `script/build-database-sidecar`.
2. Build and launch the development version of Zed.
3. Open the Database panel using its database icon in the right dock.
4. Select **New Connection**, then select SQLite in the Driver dropdown.
5. Confirm that the editor reports the driver as missing when it is not installed.
6. Select **Download Driver** and wait for the installed confirmation.
7. Keep the generated JDBC URL, or point it at a disposable file.
8. Change the name and read-only setting, then select **Test Connection**.
9. Confirm that the success message contains SQLite and JDBC driver versions.
10. Save the connection, expand it, then expand **Consoles** and select its **+**
    icon to create `console.sql` as a child item.
11. Enter the following document in the native SQL editor:

    ```sql
    select 1 as first;
    select ';' as quoted_semicolon;
    select 3 as third;
    ```

12. Put the cursor in the second statement and select **Run Selection / Current**.
    Confirm that the bottom Database Results panel opens with one result.
13. Select the first two statements and run **Run Selection / Current** again.
    Confirm that the bottom panel contains **Result 1** and **Result 2**.
14. Select **Run All** and confirm that all three result tabs are available.
15. Create `console-2.sql` with the **Consoles** group's **+** icon and confirm
    both consoles appear in the group.
16. Close and reopen Zed with the same workspace. Confirm that the connection,
    both console names, and their SQL contents remain.
17. Edit the saved profile, enter a password if the target database needs one,
    save it, then reopen the editor. The password field must remain visually
    empty while **Test Connection** continues to use the stored secret.
18. Remove the profile, restart Zed, and confirm that it and its consoles stay
    removed.

For metadata and table data:

1. Expand a connection, then expand **Databases**.
2. Expand a database and confirm that its tables and views appear. Schema names
   are included in table labels when the JDBC driver returns them.
3. Select the chevron beside a table and confirm that **Columns** and **Indexes**
   appear below it, including types, nullability, indexed columns, and uniqueness.
4. Select the table row itself and confirm that a central data tab opens with at
   most 100 rows per page.
5. Enter an expression such as `score >= 10` in **WHERE**, then press Enter.
6. Enter `created_at desc` in **ORDER BY**, then press Enter.
7. Select a column header three times. Confirm that **ORDER BY** changes to
   ascending, then descending, then empty, and that data reloads after each click.
8. Edit the connection and disable **Read-only**, then open a table with a primary
   key. Confirm that **Add Row**, **Discard**, and **Save Changes** are available.
9. Select a row and confirm that **Delete Row** becomes available. Double-click a
   cell, edit its value, then select another cell. Confirm that the
   changed cell is highlighted but that the database has not changed yet.
10. Double-click a JDBC `DATE`, `TIMESTAMP`, or `DATETIME` cell, then select the
    clock button. Choose a day in the calendar and confirm that timestamps keep
    their existing time and timezone suffix.
11. Right-click a cell and select **Set NULL**. Confirm that `NULL` is staged while
    typing the text `NULL` directly remains an ordinary string value.
12. Add a row, enter its values, mark an existing row for deletion using either
    **Delete Row** or the context menu, and select
    **Save Changes**. Confirm that all three changes appear after the automatic
    refresh.
13. Stage another edit and select **Refresh**, change page, select a column header,
    or choose **View Relation**. Confirm that Zed asks whether to discard the
    pending changes before navigating.
14. Close a table tab with a staged edit. Confirm that Zed offers to save,
    discard, or cancel instead of silently losing the change.
15. Select **Discard** directly. Confirm that original values return
    and newly staged rows disappear.
16. Use **Previous** and **Next** to navigate multiple 100-row pages.
17. Right-click a non-null foreign-key cell and select **View Relation**. Confirm
    that a new table tab opens with the referenced row and a readable `WHERE`.
18. Open a table without a primary key. Confirm that it remains browsable but the
    editing actions are unavailable and the toolbar explains why.

The first editing slice sends at most 1,000 row mutations per save and relies on
JDBC conversion from entered text to the reported column type. It intentionally
requires both a writable connection and a JDBC-reported primary key. Temporal
cells have a calendar picker; richer binary, JSON, and database-specific editors
and optimistic concurrency checks remain planned. Editing is also
disabled when the bounded result reader had to truncate a cell, so a shortened
primary-key value can never identify the wrong row.

Also create a **Custom JDBC** connection, select a local driver JAR with
**Browse**, and confirm that Zed uses it without copying or downloading it.

Then repeat the test with available PostgreSQL, MySQL, and ClickHouse instances.
Use driver-specific JDBC URLs such as:

```text
jdbc:postgresql://localhost:5432/postgres
jdbc:mysql://localhost:3306/mysql
jdbc:clickhouse://localhost:8123/default
jdbc:sqlite:database.sqlite
```

The sidecar currently starts once per connection test, metadata request, or
statement. Statements selected together therefore execute in order but do not
yet share a JDBC session or transaction. Persistent process management,
cancellation, metadata search, explicit schema nodes, and stored-routine
delimiter handling are the next runtime slices.

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

### ADR-005: drivers are classpath resources

Keep the Java sidecar database-agnostic. Predefined drivers are downloaded and
verified independently, while custom connections reference a local JAR. Start
Java with the sidecar and selected driver on its classpath, then let JDBC
`DriverManager` select the implementation from the connection URL.
