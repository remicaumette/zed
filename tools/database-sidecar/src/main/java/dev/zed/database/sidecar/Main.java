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
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.time.Duration;
import java.time.Instant;
import java.util.Map;
import java.util.Properties;
import java.util.regex.Pattern;

public final class Main {
    private static final int PROTOCOL_VERSION = 1;
    private static final int MAX_FRAME_SIZE = 1024 * 1024;
    private static final Pattern SECRET_PATTERN = Pattern.compile(
        "(?i)(password|passwd|pwd|token|secret)=([^&;\\s]+)"
    );
    private static final ObjectMapper JSON = new ObjectMapper()
        .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, true);
    private static final Map<String, String> DRIVER_CLASSES = Map.of(
        "postgresql", "org.postgresql.Driver",
        "mysql", "com.mysql.cj.jdbc.Driver",
        "click_house", "com.clickhouse.jdbc.ClickHouseDriver",
        "sqlite", "org.sqlite.JDBC"
    );

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
        if (!"testConnection".equals(request.operation)) {
            return ResponseEnvelope.error(
                request.requestId,
                "unsupported_operation",
                "Unsupported operation",
                null
            );
        }

        try {
            return ResponseEnvelope.success(request.requestId, testConnection(request.connection));
        } catch (SQLException error) {
            return ResponseEnvelope.error(
                request.requestId,
                "jdbc_error",
                sanitize(error.getMessage()),
                error.getSQLState()
            );
        } catch (ReflectiveOperationException error) {
            return ResponseEnvelope.error(
                request.requestId,
                "driver_unavailable",
                sanitize(error.getMessage()),
                null
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
        throws SQLException, ReflectiveOperationException {
        String driverClass = DRIVER_CLASSES.get(request.driver);
        if (driverClass == null) {
            throw new IllegalArgumentException("Unsupported JDBC driver");
        }
        Class.forName(driverClass);

        Properties properties = new Properties();
        if (request.username != null && !request.username.isBlank()) {
            properties.setProperty("user", request.username);
        }
        if (request.password != null && !request.password.isEmpty()) {
            properties.setProperty("password", request.password);
        }

        DriverManager.setLoginTimeout(Math.max(1, request.timeoutSeconds));
        Instant startedAt = Instant.now();
        try (Connection connection = DriverManager.getConnection(request.jdbcUrl, properties)) {
            if (!"sqlite".equals(request.driver)) {
                try {
                    connection.setReadOnly(request.readOnly);
                } catch (SQLFeatureNotSupportedException | UnsupportedOperationException ignored) {
                    // Read-only is also enforced by the query executor. This flag is best effort.
                }
            }

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
    }

    public static final class ConnectionRequest {
        public String driver;
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
        public ConnectionTestResult result;
        public SidecarError error;

        static ResponseEnvelope success(String requestId, ConnectionTestResult result) {
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
