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

pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    // Both this crate and the root rise-deploy crate share the same `_sqlx_migrations`
    // table. Without ignore_missing, this migrator would error on root migrations it
    // doesn't know about (and vice versa). Each migrator only manages its own set.
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}
