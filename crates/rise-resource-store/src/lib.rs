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
    CollectionInfo, CreateResourceParams, DeleteOutcome, PathSegment, PropagationPolicy,
    ResourceStore, UpdateResourceParams, CASCADE_DELETION_FINALIZER, SYSTEM_FINALIZER_PREFIX,
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
/// sqlx 0.8 hard-codes the unqualified `_sqlx_migrations` name, so we acquire a
/// dedicated connection, ensure the schema exists, switch `search_path` so the
/// migrator resolves `_sqlx_migrations` inside `resource_store`, run migrations,
/// then reset `search_path` before the connection returns to the pool.
pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    use sqlx::Executor;

    let mut conn = pool.acquire().await?;

    conn.execute("CREATE SCHEMA IF NOT EXISTS resource_store")
        .await?;
    conn.execute("SET search_path TO resource_store, public")
        .await?;

    let result = sqlx::migrate!("./migrations").run(&mut *conn).await;

    // Reset before the connection returns to the pool so other consumers see the
    // default search_path. Best-effort: if migrate failed we still try to reset.
    let _ = conn.execute("RESET search_path").await;

    result
}
