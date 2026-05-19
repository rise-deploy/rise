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
    CollectionInfo, CreateResourceParams, DeleteOutcome, ResourceStore, UpdateResourceParams,
};
pub use validation::{
    JsonSchemaValidator, NoOpValidator, OrganizationValidator, ResourceDefinitionValidator,
    SpecValidator,
};

pub async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
