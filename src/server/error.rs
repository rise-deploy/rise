use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Server error type that provides automatic logging and clean error responses.
///
/// This type:
/// - Automatically logs errors when converted to HTTP responses (via IntoResponse)
/// - Preserves full error chains from anyhow::Error for debugging
/// - Allows attaching structured context (user IDs, project names, etc.)
/// - Returns clean, user-friendly error messages to clients
///
/// # Example
///
/// ```rust,ignore
/// use crate::server::error::{ServerError, ServerErrorExt};
///
/// // Simple error with just a message
/// let err = ServerError::bad_request("Invalid project name");
///
/// // Error from anyhow with context
/// let result: Result<_, anyhow::Error> = fetch_user();
/// let user = result
///     .internal_err("Failed to fetch user")
///     .map_err(|e| e.with_context("user_id", user_id.to_string()))?;
///
/// // Error with full context
/// let err = ServerError::from_anyhow(
///     anyhow!("Database connection failed"),
///     StatusCode::INTERNAL_SERVER_ERROR,
///     "Failed to connect to database"
/// )
/// .with_context("operation", "create_project")
/// .with_context("project_name", &project_name);
/// ```
#[derive(Debug)]
pub struct ServerError {
    /// HTTP status code to return
    pub status: StatusCode,
    /// User-facing error message (returned in response)
    pub message: String,
    /// Internal error with full chain (logged but not exposed to client)
    pub source: Option<anyhow::Error>,
    /// Structured context for logging (key-value pairs)
    pub context: Vec<(&'static str, String)>,
    /// Optional suggestions for the client (e.g. fuzzy-matched names)
    pub suggestions: Option<Vec<String>>,
    /// If true, skip error-level logging (for expected transient conditions)
    pub expected: bool,
    /// If true, the operation lost a `SERIALIZABLE` race and the caller may
    /// replay it from the beginning (ADR-0001 §5).
    ///
    /// This rides on the error rather than on a separate result type because a
    /// serialization failure surfaces from deep inside the write path — a store
    /// call several helpers down — and every layer between there and the retry
    /// loop already speaks `ServerError`. A retry loop that guessed from the
    /// status code would replay writes that merely happened to return 503.
    pub retryable: bool,
}

impl ServerError {
    /// Create a new error with just status and message (no source error)
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            source: None,
            context: Vec::new(),
            suggestions: None,
            expected: false,
            retryable: false,
        }
    }

    /// Create an error from an anyhow::Error with full error chain
    pub fn from_anyhow(
        source: anyhow::Error,
        status: StatusCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            source: Some(source),
            context: Vec::new(),
            suggestions: None,
            expected: false,
            retryable: false,
        }
    }

    /// Add a context field for logging (chainable)
    pub fn with_context(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.context.push((key, value.into()));
        self
    }

    /// Add suggestions to the error response (chainable)
    pub fn with_suggestions(mut self, suggestions: Option<Vec<String>>) -> Self {
        self.suggestions = suggestions;
        self
    }

    /// Mark this error as replayable by a bounded retry loop (chainable).
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// Mark this error as expected, suppressing ERROR-level logging (chainable)
    ///
    /// Use for transient conditions that are normal (e.g. "pod not ready yet")
    /// rather than actual failures. These are logged at WARN instead of ERROR.
    pub fn expected(mut self) -> Self {
        self.expected = true;
        self
    }

    /// Create a 500 Internal Server Error
    #[allow(dead_code)]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// Create a 500 Internal Server Error from an anyhow::Error
    pub fn internal_anyhow(source: anyhow::Error, message: impl Into<String>) -> Self {
        Self::from_anyhow(source, StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// Create a 401 Unauthorized error
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    /// Create a 400 Bad Request error
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// Create a 403 Forbidden error
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    /// Create a 404 Not Found error
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// Create a 409 Conflict error
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// Create a 410 Gone error
    pub fn gone(message: impl Into<String>) -> Self {
        Self::new(StatusCode::GONE, message)
    }

    /// Create a 503 Service Unavailable error
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        // Log server errors (5xx) with full context using structured fields
        // Errors marked as `expected` are logged at WARN (transient conditions
        // like "pod not ready yet"), all others at ERROR.
        if self.status.is_server_error() {
            if self.expected {
                if let Some(source) = &self.source {
                    tracing::warn!(
                        status = self.status.as_u16(),
                        message = %self.message,
                        context = ?self.context,
                        error = ?source,
                        "Expected server error"
                    );
                } else {
                    tracing::warn!(
                        status = self.status.as_u16(),
                        message = %self.message,
                        context = ?self.context,
                        "Expected server error"
                    );
                }
            } else if let Some(source) = &self.source {
                tracing::error!(
                    status = self.status.as_u16(),
                    message = %self.message,
                    context = ?self.context,
                    error = ?source,
                    "Server error"
                );
            } else {
                tracing::error!(
                    status = self.status.as_u16(),
                    message = %self.message,
                    context = ?self.context,
                    "Server error"
                );
            }
        }

        // Return clean JSON error response to client
        let body = if let Some(suggestions) = &self.suggestions {
            Json(json!({
                "error": self.message,
                "suggestions": suggestions,
            }))
        } else {
            Json(json!({
                "error": self.message,
            }))
        };

        (self.status, body).into_response()
    }
}

// Implement From for common error types
impl From<sqlx::Error> for ServerError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal_anyhow(err.into(), "Database operation failed")
    }
}

impl From<anyhow::Error> for ServerError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal_anyhow(err, "Internal server error")
    }
}

impl From<(StatusCode, String)> for ServerError {
    fn from((status, message): (StatusCode, String)) -> Self {
        Self::new(status, message)
    }
}

impl From<ServerError> for (StatusCode, String) {
    fn from(err: ServerError) -> Self {
        (err.status, err.message)
    }
}

/// Extension trait for Result types to easily convert to ServerError
///
/// This trait provides ergonomic methods for converting Result<T, E> to Result<T, ServerError>
/// where E can be converted to anyhow::Error.
///
/// # Example
///
/// ```rust,ignore
/// use crate::server::error::ServerErrorExt;
///
/// // Convert with custom status and message
/// let result = some_operation()
///     .server_err(StatusCode::BAD_REQUEST, "Invalid operation")?;
///
/// // Convert to internal server error (500)
/// let result = database_query()
///     .internal_err("Failed to query database")?;
/// ```
pub trait ServerErrorExt<T> {
    /// Convert error to ServerError with custom status and message
    fn server_err(self, status: StatusCode, message: impl Into<String>) -> Result<T, ServerError>;

    /// Convert error to internal server error (500)
    fn internal_err(self, message: impl Into<String>) -> Result<T, ServerError>;
}

impl<T, E> ServerErrorExt<T> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn server_err(self, status: StatusCode, message: impl Into<String>) -> Result<T, ServerError> {
        self.map_err(|e| ServerError::from_anyhow(e.into(), status, message))
    }

    fn internal_err(self, message: impl Into<String>) -> Result<T, ServerError> {
        self.map_err(|e| ServerError::internal_anyhow(e.into(), message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn test_server_error_response_shape() {
        // Test that ServerError returns expected JSON response
        let error = ServerError::bad_request("Invalid input");
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Extract and verify JSON body
        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body_json["error"], "Invalid input");
    }

    #[tokio::test]
    async fn test_server_error_with_context() {
        let error = ServerError::internal("Database error")
            .with_context("user_id", "123")
            .with_context("operation", "fetch_user");

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body_json["error"], "Database error");
    }

    #[tokio::test]
    async fn test_server_error_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("Something went wrong");
        let error = ServerError::from_anyhow(
            anyhow_err,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Operation failed",
        );

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body_json["error"], "Operation failed");
    }

    #[tokio::test]
    async fn test_server_error_status_codes() {
        // Test various status code helpers
        let bad_request = ServerError::bad_request("Bad input");
        assert_eq!(bad_request.status, StatusCode::BAD_REQUEST);

        let forbidden = ServerError::forbidden("Access denied");
        assert_eq!(forbidden.status, StatusCode::FORBIDDEN);

        let not_found = ServerError::not_found("Resource missing");
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);

        let internal = ServerError::internal("Server error");
        assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);

        let conflict = ServerError::conflict("Already exists");
        assert_eq!(conflict.status, StatusCode::CONFLICT);

        let gone = ServerError::gone("No longer available");
        assert_eq!(gone.status, StatusCode::GONE);

        let unavailable = ServerError::service_unavailable("Try later");
        assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_server_error_with_suggestions() {
        let error = ServerError::not_found("Team 'devopsy' not found")
            .with_suggestions(Some(vec!["devops".to_string(), "dev-ops".to_string()]));

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(body_json["error"], "Team 'devopsy' not found");
        assert_eq!(body_json["suggestions"][0], "devops");
        assert_eq!(body_json["suggestions"][1], "dev-ops");
    }

    #[tokio::test]
    async fn test_server_error_without_suggestions_has_no_suggestions_key() {
        let error = ServerError::not_found("Not found");
        let response = error.into_response();

        let body_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert!(body_json.get("suggestions").is_none());
    }

    #[tokio::test]
    async fn test_server_error_ext_trait() {
        // Test ServerErrorExt trait methods
        let result: Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));

        let error = result
            .server_err(StatusCode::NOT_FOUND, "File operation failed")
            .unwrap_err();

        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.message, "File operation failed");

        // Test internal_err helper
        let result2: Result<(), std::io::Error> = Err(std::io::Error::other("io error"));

        let error2 = result2
            .internal_err("Internal operation failed")
            .unwrap_err();

        assert_eq!(error2.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error2.message, "Internal operation failed");
    }
}
