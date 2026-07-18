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
import java.util.List;
import java.util.Properties;
import java.util.regex.Pattern;

public final class Main {
    private static final int PROTOCOL_VERSION = 1;
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
        try (Connection connection = openConnection(request.connection);
             Statement statement = connection.createStatement()) {
            try {
                statement.setQueryTimeout(Math.max(1, request.connection.timeoutSeconds));
            } catch (SQLException | UnsupportedOperationException ignored) {
                // Query timeouts are optional in JDBC drivers.
            }
            try {
                statement.setMaxRows(request.maxRows == Integer.MAX_VALUE
                    ? request.maxRows
                    : request.maxRows + 1);
            } catch (SQLException | UnsupportedOperationException ignored) {
                // The result reader still enforces the row limit.
            }

            boolean hasResultSet = statement.execute(request.sql);
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
                    if (rows.size() >= request.maxRows) {
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
