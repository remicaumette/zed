package dev.zed.database.sidecar;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.nio.charset.StandardCharsets;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Properties;
import java.util.Set;
import java.util.TreeMap;
import java.util.regex.Pattern;

public final class Main {
    private static final int PROTOCOL_VERSION = 2;
    private static final int MAX_FRAME_SIZE = 16 * 1024 * 1024;
    private static final int MAX_CELL_CHARACTERS = 4096;
    private static final int MAX_RESULT_CHARACTERS = 500_000;
    private static final Pattern SECRET_PATTERN = Pattern.compile(
        "(?i)(password|passwd|pwd|token|secret)=([^&;\\s]+)"
    );
    private static final ObjectMapper JSON = new ObjectMapper()
        .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, true);
    private Main() {}

    public static void main(String[] args) throws Exception {
        try (DataInputStream input = new DataInputStream(new BufferedInputStream(System.in));
             DataOutputStream output = new DataOutputStream(new BufferedOutputStream(System.out))) {
            while (true) {
                byte[] frame;
                try {
                    frame = readFrame(input);
                } catch (EOFException ignored) {
                    return;
                }

                RequestEnvelope request = JSON.readValue(frame, RequestEnvelope.class);
                ResponseEnvelope response = handle(request);
                writeFrame(output, JSON.writeValueAsBytes(response));
            }
        }
    }

    private static ResponseEnvelope handle(RequestEnvelope request) {
        if (request.protocolVersion != PROTOCOL_VERSION) {
            return ResponseEnvelope.error(
                request.requestId,
                "protocol_mismatch",
                "Expected protocol version " + PROTOCOL_VERSION,
                null
            );
        }
        try {
            switch (request.operation) {
                case "testConnection":
                    return ResponseEnvelope.success(
                        request.requestId,
                        testConnection(request.connection)
                    );
                case "execute":
                    return ResponseEnvelope.success(request.requestId, execute(request));
                case "listDatabases":
                    return ResponseEnvelope.success(
                        request.requestId,
                        listDatabases(request.connection)
                    );
                case "listTables":
                    return ResponseEnvelope.success(request.requestId, listTables(request));
                case "describeTable":
                    return ResponseEnvelope.success(request.requestId, describeTable(request));
                case "browseTable":
                    return ResponseEnvelope.success(request.requestId, browseTable(request));
                default:
                    return ResponseEnvelope.error(
                        request.requestId,
                        "unsupported_operation",
                        "Unsupported operation",
                        null
                    );
            }
        } catch (SQLException error) {
            return ResponseEnvelope.error(
                request.requestId,
                "jdbc_error",
                sanitize(error.getMessage()),
                error.getSQLState()
            );
        } catch (RuntimeException error) {
            return ResponseEnvelope.error(
                request.requestId,
                "sidecar_error",
                sanitize(error.getMessage()),
                null
            );
        }
    }

    private static ConnectionTestResult testConnection(ConnectionRequest request)
        throws SQLException {
        DriverManager.setLoginTimeout(Math.max(1, request.timeoutSeconds));
        Instant startedAt = Instant.now();
        try (Connection connection = openConnection(request)) {

            if (!connection.isValid(Math.max(1, request.timeoutSeconds))) {
                throw new SQLException("The driver reported an invalid connection");
            }

            DatabaseMetaData metadata = connection.getMetaData();
            return new ConnectionTestResult(
                metadata.getDatabaseProductName(),
                metadata.getDatabaseProductVersion(),
                metadata.getDriverName(),
                metadata.getDriverVersion(),
                Duration.between(startedAt, Instant.now()).toMillis()
            );
        }
    }

    private static QueryResult execute(RequestEnvelope request) throws SQLException {
        if (request.sql == null || request.sql.isBlank()) {
            throw new IllegalArgumentException("SQL cannot be empty");
        }
        if (request.maxRows <= 0) {
            throw new IllegalArgumentException("maxRows must be greater than zero");
        }

        DriverManager.setLoginTimeout(Math.max(1, request.connection.timeoutSeconds));
        Instant startedAt = Instant.now();
        try (Connection connection = openConnection(request.connection)) {
            return execute(
                connection,
                request.sql,
                request.maxRows,
                request.connection.timeoutSeconds,
                startedAt
            );
        }
    }

    private static QueryResult browseTable(RequestEnvelope request) throws SQLException {
        if (request.table == null || request.table.isBlank()) {
            throw new IllegalArgumentException("A table name is required");
        }
        if (request.maxRows <= 0) {
            throw new IllegalArgumentException("maxRows must be greater than zero");
        }

        DriverManager.setLoginTimeout(Math.max(1, request.connection.timeoutSeconds));
        Instant startedAt = Instant.now();
        try (Connection connection = openConnection(request.connection)) {
            StringBuilder sql = new StringBuilder("SELECT * FROM ")
                .append(qualifiedTableName(connection, request));
            if (request.whereClause != null && !request.whereClause.isBlank()) {
                sql.append(" WHERE ").append(request.whereClause.trim());
            }
            if (request.orderBy != null && !request.orderBy.isBlank()) {
                sql.append(" ORDER BY ").append(request.orderBy.trim());
            }
            return execute(
                connection,
                sql.toString(),
                request.maxRows,
                request.connection.timeoutSeconds,
                startedAt
            );
        }
    }

    private static QueryResult execute(
        Connection connection,
        String sql,
        int maxRows,
        int timeoutSeconds,
        Instant startedAt
    ) throws SQLException {
        try (Statement statement = connection.createStatement()) {
            try {
                statement.setQueryTimeout(Math.max(1, timeoutSeconds));
            } catch (SQLException | UnsupportedOperationException ignored) {
                // Query timeouts are optional in JDBC drivers.
            }
            try {
                statement.setMaxRows(maxRows == Integer.MAX_VALUE ? maxRows : maxRows + 1);
            } catch (SQLException | UnsupportedOperationException ignored) {
                // The result reader still enforces the row limit.
            }

            boolean hasResultSet = statement.execute(sql);
            if (!hasResultSet) {
                int updateCount = statement.getUpdateCount();
                return new QueryResult(
                    List.of(),
                    List.of(),
                    updateCount >= 0 ? Long.valueOf(updateCount) : null,
                    false,
                    Duration.between(startedAt, Instant.now()).toMillis()
                );
            }

            try (ResultSet resultSet = statement.getResultSet()) {
                ResultSetMetaData metadata = resultSet.getMetaData();
                int columnCount = metadata.getColumnCount();
                List<QueryColumn> columns = new ArrayList<>(columnCount);
                for (int column = 1; column <= columnCount; column++) {
                    String label = metadata.getColumnLabel(column);
                    if (label == null || label.isBlank()) {
                        label = metadata.getColumnName(column);
                    }
                    columns.add(new QueryColumn(label, metadata.getColumnTypeName(column)));
                }

                List<List<String>> rows = new ArrayList<>();
                boolean truncated = false;
                int remainingCharacters = MAX_RESULT_CHARACTERS;
                rowsLoop:
                while (resultSet.next()) {
                    if (rows.size() >= maxRows) {
                        truncated = true;
                        break;
                    }

                    List<String> row = new ArrayList<>(columnCount);
                    for (int column = 1; column <= columnCount; column++) {
                        String value = resultSet.getString(column);
                        if (value != null && value.length() > MAX_CELL_CHARACTERS) {
                            value = value.substring(0, MAX_CELL_CHARACTERS);
                            truncated = true;
                        }
                        if (value != null && value.length() > remainingCharacters) {
                            value = value.substring(0, remainingCharacters);
                            truncated = true;
                        }
                        row.add(value);
                        if (value != null) {
                            remainingCharacters -= value.length();
                        }
                    }
                    rows.add(row);
                    if (remainingCharacters == 0) {
                        truncated = true;
                        break rowsLoop;
                    }
                }

                return new QueryResult(
                    columns,
                    rows,
                    null,
                    truncated,
                    Duration.between(startedAt, Instant.now()).toMillis()
                );
            }
        }
    }

    private static List<MetadataDatabase> listDatabases(ConnectionRequest request)
        throws SQLException {
        DriverManager.setLoginTimeout(Math.max(1, request.timeoutSeconds));
        try (Connection connection = openConnection(request)) {
            DatabaseMetaData metadata = connection.getMetaData();
            List<MetadataDatabase> databases = new ArrayList<>();
            Set<String> seen = new HashSet<>();

            try (ResultSet catalogs = metadata.getCatalogs()) {
                while (catalogs.next()) {
                    String catalog = catalogs.getString(1);
                    addDatabase(databases, seen, catalog, catalog, null);
                }
            }

            if (databases.isEmpty()) {
                try (ResultSet schemas = metadata.getSchemas()) {
                    while (schemas.next()) {
                        String schema = schemas.getString("TABLE_SCHEM");
                        String catalog = nullableString(schemas, "TABLE_CATALOG");
                        addDatabase(databases, seen, schema, catalog, schema);
                    }
                }
            }

            if (databases.isEmpty()) {
                String catalog = connection.getCatalog();
                String schema = null;
                try {
                    schema = connection.getSchema();
                } catch (SQLException | UnsupportedOperationException ignored) {
                    // Connection schemas are optional.
                }
                String name = !isBlank(catalog) ? catalog : (!isBlank(schema) ? schema : "main");
                addDatabase(databases, seen, name, catalog, schema);
            }

            databases.sort(Comparator.comparing(
                database -> database.name,
                String.CASE_INSENSITIVE_ORDER
            ));
            return databases;
        }
    }

    private static void addDatabase(
        List<MetadataDatabase> databases,
        Set<String> seen,
        String name,
        String catalog,
        String schema
    ) {
        if (isBlank(name)) {
            return;
        }
        String key = String.valueOf(catalog) + '\0' + String.valueOf(schema);
        if (seen.add(key)) {
            databases.add(new MetadataDatabase(name, catalog, schema));
        }
    }

    private static List<MetadataTable> listTables(RequestEnvelope request) throws SQLException {
        DriverManager.setLoginTimeout(Math.max(1, request.connection.timeoutSeconds));
        try (Connection connection = openConnection(request.connection)) {
            DatabaseMetaData metadata = connection.getMetaData();
            String identifierQuote = identifierQuote(metadata);
            try (ResultSet tables = metadata.getTables(
                 request.catalog,
                 request.schema,
                 "%",
                 null
             )) {
                List<MetadataTable> result = new ArrayList<>();
                while (tables.next()) {
                    String name = tables.getString("TABLE_NAME");
                    String tableType = tables.getString("TABLE_TYPE");
                    if (isBlank(name) || !isBrowsableTableType(tableType)) {
                        continue;
                    }
                    result.add(new MetadataTable(
                        nullableString(tables, "TABLE_CAT"),
                        nullableString(tables, "TABLE_SCHEM"),
                        name,
                        tableType,
                        identifierQuote
                    ));
                }
                result.sort(
                    Comparator.comparing(
                        (MetadataTable table) -> nullToEmpty(table.schema),
                        String.CASE_INSENSITIVE_ORDER
                    ).thenComparing(table -> table.name, String.CASE_INSENSITIVE_ORDER)
                );
                return result;
            }
        }
    }

    private static TableMetadataDetails describeTable(RequestEnvelope request)
        throws SQLException {
        if (request.table == null || request.table.isBlank()) {
            throw new IllegalArgumentException("A table name is required");
        }
        DriverManager.setLoginTimeout(Math.max(1, request.connection.timeoutSeconds));
        try (Connection connection = openConnection(request.connection)) {
            DatabaseMetaData metadata = connection.getMetaData();
            List<MetadataColumn> columns = new ArrayList<>();
            try (ResultSet result = metadata.getColumns(
                request.catalog,
                request.schema,
                request.table,
                "%"
            )) {
                while (result.next()) {
                    columns.add(new MetadataColumn(
                        result.getString("COLUMN_NAME"),
                        result.getString("TYPE_NAME"),
                        result.getInt("NULLABLE") != DatabaseMetaData.columnNoNulls,
                        result.getInt("ORDINAL_POSITION"),
                        nullableString(result, "COLUMN_DEF")
                    ));
                }
            }
            columns.sort(Comparator.comparingInt(column -> column.ordinalPosition));

            Map<String, IndexAccumulator> accumulators = new LinkedHashMap<>();
            try {
                try (ResultSet result = metadata.getIndexInfo(
                    request.catalog,
                    request.schema,
                    request.table,
                    false,
                    true
                )) {
                    while (result.next()) {
                        if (result.getShort("TYPE") == DatabaseMetaData.tableIndexStatistic) {
                            continue;
                        }
                        String name = nullableString(result, "INDEX_NAME");
                        String column = nullableString(result, "COLUMN_NAME");
                        if (isBlank(name) || isBlank(column)) {
                            continue;
                        }
                        IndexAccumulator index = accumulators.computeIfAbsent(
                            name,
                            key -> new IndexAccumulator(key, !getBoolean(result, "NON_UNIQUE"))
                        );
                        index.columns.put((int) result.getShort("ORDINAL_POSITION"), column);
                    }
                }
            } catch (SQLException | UnsupportedOperationException ignored) {
                // Index metadata is optional for custom and analytical JDBC drivers.
            }

            List<MetadataIndex> indexes = new ArrayList<>();
            for (IndexAccumulator accumulator : accumulators.values()) {
                indexes.add(new MetadataIndex(
                    accumulator.name,
                    accumulator.unique,
                    new ArrayList<>(accumulator.columns.values())
                ));
            }
            indexes.sort(Comparator.comparing(index -> index.name, String.CASE_INSENSITIVE_ORDER));
            return new TableMetadataDetails(columns, indexes);
        }
    }

    private static String qualifiedTableName(Connection connection, RequestEnvelope request)
        throws SQLException {
        DatabaseMetaData metadata = connection.getMetaData();
        String tableName = quoteIdentifier(metadata, request.table);

        if (!isBlank(request.schema) && metadata.supportsSchemasInDataManipulation()) {
            tableName = quoteIdentifier(metadata, request.schema) + "." + tableName;
        }
        if (!isBlank(request.catalog) && metadata.supportsCatalogsInDataManipulation()) {
            String catalog = quoteIdentifier(metadata, request.catalog);
            String separator = metadata.getCatalogSeparator();
            if (isBlank(separator)) {
                separator = ".";
            }
            tableName = metadata.isCatalogAtStart()
                ? catalog + separator + tableName
                : tableName + separator + catalog;
        }
        return tableName;
    }

    private static String quoteIdentifier(DatabaseMetaData metadata, String identifier)
        throws SQLException {
        String quote = identifierQuote(metadata);
        if (quote == null) {
            return identifier;
        }
        return quote + identifier.replace(quote, quote + quote) + quote;
    }

    private static String identifierQuote(DatabaseMetaData metadata) throws SQLException {
        String quote = metadata.getIdentifierQuoteString();
        return isBlank(quote) ? null : quote.trim();
    }

    private static String nullableString(ResultSet result, String column) {
        try {
            return result.getString(column);
        } catch (SQLException ignored) {
            return null;
        }
    }

    private static boolean getBoolean(ResultSet result, String column) {
        try {
            return result.getBoolean(column);
        } catch (SQLException error) {
            throw new IllegalStateException(error);
        }
    }

    private static boolean isBlank(String value) {
        return value == null || value.isBlank();
    }

    private static boolean isBrowsableTableType(String tableType) {
        if (isBlank(tableType)) {
            return false;
        }
        String normalized = tableType.toUpperCase(Locale.ROOT);
        return !normalized.startsWith("SYSTEM")
            && (normalized.contains("TABLE") || normalized.contains("VIEW"));
    }

    private static String nullToEmpty(String value) {
        return value == null ? "" : value;
    }

    private static Connection openConnection(ConnectionRequest request) throws SQLException {
        Properties properties = new Properties();
        if (request.username != null && !request.username.isBlank()) {
            properties.setProperty("user", request.username);
        }
        if (request.password != null && !request.password.isEmpty()) {
            properties.setProperty("password", request.password);
        }

        Connection connection = DriverManager.getConnection(request.jdbcUrl, properties);
        try {
            connection.setReadOnly(request.readOnly);
        } catch (SQLException | UnsupportedOperationException ignored) {
            // Some JDBC drivers do not support this hint.
        }
        return connection;
    }

    private static byte[] readFrame(DataInputStream input) throws Exception {
        int length = input.readInt();
        if (length < 0 || length > MAX_FRAME_SIZE) {
            throw new IllegalArgumentException("Invalid request frame length");
        }
        byte[] payload = new byte[length];
        input.readFully(payload);
        return payload;
    }

    private static void writeFrame(DataOutputStream output, byte[] payload) throws Exception {
        if (payload.length > MAX_FRAME_SIZE) {
            throw new IllegalArgumentException("Response frame is too large");
        }
        output.writeInt(payload.length);
        output.write(payload);
        output.flush();
    }

    private static String sanitize(String message) {
        if (message == null || message.isBlank()) {
            return "Unknown JDBC error";
        }
        return SECRET_PATTERN.matcher(message).replaceAll("$1=<redacted>");
    }

    public static final class RequestEnvelope {
        public int protocolVersion;
        public String requestId;
        public String operation;
        public ConnectionRequest connection;
        public String sql;
        public int maxRows;
        public String catalog;
        public String schema;
        public String table;
        public String whereClause;
        public String orderBy;
    }

    public static final class ConnectionRequest {
        public String jdbcUrl;
        public String username;
        public String password;
        public boolean readOnly;
        public int timeoutSeconds;
    }

    public static final class ResponseEnvelope {
        public int protocolVersion = PROTOCOL_VERSION;
        public String requestId;
        public boolean ok;
        public Object result;
        public SidecarError error;

        static ResponseEnvelope success(String requestId, Object result) {
            ResponseEnvelope response = new ResponseEnvelope();
            response.requestId = requestId;
            response.ok = true;
            response.result = result;
            return response;
        }

        static ResponseEnvelope error(
            String requestId,
            String code,
            String message,
            String sqlState
        ) {
            ResponseEnvelope response = new ResponseEnvelope();
            response.requestId = requestId;
            response.ok = false;
            response.error = new SidecarError(code, message, sqlState);
            return response;
        }
    }

    public static final class ConnectionTestResult {
        public final String databaseProduct;
        public final String databaseVersion;
        public final String driverName;
        public final String driverVersion;
        public final long roundTripMillis;

        ConnectionTestResult(
            String databaseProduct,
            String databaseVersion,
            String driverName,
            String driverVersion,
            long roundTripMillis
        ) {
            this.databaseProduct = databaseProduct;
            this.databaseVersion = databaseVersion;
            this.driverName = driverName;
            this.driverVersion = driverVersion;
            this.roundTripMillis = roundTripMillis;
        }
    }

    public static final class QueryColumn {
        public final String label;
        public final String typeName;

        QueryColumn(String label, String typeName) {
            this.label = label;
            this.typeName = typeName;
        }
    }

    public static final class QueryResult {
        public final List<QueryColumn> columns;
        public final List<List<String>> rows;
        public final Long affectedRows;
        public final boolean truncated;
        public final long elapsedMillis;

        QueryResult(
            List<QueryColumn> columns,
            List<List<String>> rows,
            Long affectedRows,
            boolean truncated,
            long elapsedMillis
        ) {
            this.columns = columns;
            this.rows = rows;
            this.affectedRows = affectedRows;
            this.truncated = truncated;
            this.elapsedMillis = elapsedMillis;
        }
    }

    public static final class MetadataDatabase {
        public final String name;
        public final String catalog;
        public final String schema;

        MetadataDatabase(String name, String catalog, String schema) {
            this.name = name;
            this.catalog = catalog;
            this.schema = schema;
        }
    }

    public static final class MetadataTable {
        public final String catalog;
        public final String schema;
        public final String name;
        public final String tableType;
        public final String identifierQuote;

        MetadataTable(
            String catalog,
            String schema,
            String name,
            String tableType,
            String identifierQuote
        ) {
            this.catalog = catalog;
            this.schema = schema;
            this.name = name;
            this.tableType = tableType;
            this.identifierQuote = identifierQuote;
        }
    }

    public static final class MetadataColumn {
        public final String name;
        public final String typeName;
        public final boolean nullable;
        public final int ordinalPosition;
        public final String defaultValue;

        MetadataColumn(
            String name,
            String typeName,
            boolean nullable,
            int ordinalPosition,
            String defaultValue
        ) {
            this.name = name;
            this.typeName = typeName;
            this.nullable = nullable;
            this.ordinalPosition = ordinalPosition;
            this.defaultValue = defaultValue;
        }
    }

    public static final class MetadataIndex {
        public final String name;
        public final boolean unique;
        public final List<String> columns;

        MetadataIndex(String name, boolean unique, List<String> columns) {
            this.name = name;
            this.unique = unique;
            this.columns = columns;
        }
    }

    public static final class TableMetadataDetails {
        public final List<MetadataColumn> columns;
        public final List<MetadataIndex> indexes;

        TableMetadataDetails(List<MetadataColumn> columns, List<MetadataIndex> indexes) {
            this.columns = columns;
            this.indexes = indexes;
        }
    }

    private static final class IndexAccumulator {
        final String name;
        final boolean unique;
        final Map<Integer, String> columns = new TreeMap<>();

        IndexAccumulator(String name, boolean unique) {
            this.name = name;
            this.unique = unique;
        }
    }

    public static final class SidecarError {
        public final String code;
        public final String message;
        public final String sqlState;

        SidecarError(String code, String message, String sqlState) {
            this.code = code;
            this.message = message;
            this.sqlState = sqlState;
        }
    }
}
