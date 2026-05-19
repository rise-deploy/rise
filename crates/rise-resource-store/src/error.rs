use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("resource not found")]
    NotFound,

    #[error("revision conflict: expected {expected}, found {found}")]
    RevisionConflict { expected: i64, found: i64 },

    #[error("a resource with this name already exists in this scope")]
    NameConflict,

    #[error("could not generate a unique discriminator after maximum retries")]
    DiscriminatorExhausted,

    #[error("path segment kind mismatch: expected '{expected}', got '{got}'")]
    KindMismatch { expected: String, got: String },

    #[error("intermediate path segment not found")]
    ParentNotFound,

    #[error("reparent would create a cycle")]
    ReparentCycle,

    #[error("reserved finalizer namespace: '{0}'")]
    ReservedFinalizer(String),

    #[error("path resolution requires at least one segment")]
    EmptyPath,

    #[error("validation error: {0}")]
    Validation(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}
