pub mod error;
pub mod models;
pub mod pg_store;
pub mod store;
pub mod validation;

mod discriminator;

pub use error::StoreError;
pub use models::ResourceRow;
pub use pg_store::PgResourceStore;
pub use store::{
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, ResourceStore,
    UpdateResourceParams, CASCADE_DELETION_FINALIZER, MAX_PARENT_CHAIN_DEPTH,
    SYSTEM_FINALIZER_PREFIX,
};
pub use validation::{
    JsonSchemaValidator, NoOpValidator, OrganizationValidator, ResourceDefinitionValidator,
    SpecValidator,
};

/// Run resource-store migrations in their own Postgres schema (`resource_store`),
/// keeping both the application tables and the `_sqlx_migrations` tracking table
/// isolated from the root rise-deploy crate, which owns its own migrations against
/// the same database.
///
/// sqlx 0.8 hard-codes the unqualified `_sqlx_migrations` name, so we switch
/// `search_path` to make the migrator resolve it inside `resource_store`. That
/// mutates session state, so we run on a *detached* connection — closed when
/// we're done rather than returned to the pool — which guarantees the modified
/// `search_path` can never leak to another pool consumer (no best-effort reset
/// that a failure could skip).
pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    use sqlx::Executor;

    // Detach from the pool: this connection is sacrificed (closed on drop), so
    // the `SET search_path` below cannot poison a recycled connection.
    let mut conn = pool.acquire().await?.detach();

    conn.execute("CREATE SCHEMA IF NOT EXISTS resource_store")
        .await?;
    conn.execute("SET search_path TO resource_store, public")
        .await?;

    sqlx::migrate!("./migrations").run(&mut conn).await
}
