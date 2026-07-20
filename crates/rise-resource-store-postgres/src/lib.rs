mod admission;
pub mod builtin;
mod lookup;
pub mod pg_store;
pub mod validation;

mod discriminator;
mod models;

pub use builtin::{BuiltInRegistration, BuiltInRegistry};
pub use lookup::{
    GroupMembershipFact, IdentityLookup, MembershipLookup, TrustPolicyFact, TrustPolicyLookup,
    UserIdentityFact,
};
pub use pg_store::PgResourceStore;
pub use validation::{JsonSchemaValidator, OrganizationValidator, ResourceDefinitionValidator};

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
    use sqlx::migrate::Migrate;
    use sqlx::Executor;

    // Detach from the pool: this connection is sacrificed (closed on drop), so
    // the `SET search_path` below cannot poison a recycled connection.
    let mut conn = pool.acquire().await?.detach();

    conn.execute("CREATE SCHEMA IF NOT EXISTS resource_store")
        .await?;
    conn.execute("SET search_path TO resource_store, public")
        .await?;
    // Use SQLx's public lock implementation so recovery serializes with both
    // this migration pass and older Rise binaries without copying SQLx's
    // private lock-ID algorithm.
    conn.lock().await?;

    recover_concurrent_indexes(&mut conn).await?;

    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_locking(false);
    let result = migrator.run(&mut conn).await;
    let unlock = conn.unlock().await;
    result?;
    unlock?;
    Ok(())
}

struct ConcurrentIndexMigration {
    version: i64,
    name: &'static str,
    drop_sql: &'static str,
    unique: bool,
    keys: &'static [&'static str],
    collations: &'static [&'static str],
    predicate: &'static str,
}

const CONCURRENT_INDEX_MIGRATIONS: &[ConcurrentIndexMigration] = &[
    ConcurrentIndexMigration {
        version: 20260719000001,
        name: "resources_owner_references_gin",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.resources_owner_references_gin",
        unique: false,
        keys: &["owner_references"],
        collations: &[""],
        predicate: "",
    },
    ConcurrentIndexMigration {
        version: 20260719000006,
        name: "user_identities_issuer_subject_unique",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.user_identities_issuer_subject_unique",
        unique: true,
        keys: &["(spec->>'issuer')", "(spec->>'subject')"],
        collations: &["pg_catalog.C", "pg_catalog.C"],
        predicate: "api_version='rise.dev/v1alpha1'ANDsplit_part(api_version,'/',1)='rise.dev'ANDkind='UserIdentity'ANDdeletion_timestampISNULL",
    },
    ConcurrentIndexMigration {
        version: 20260719000007,
        name: "workload_trust_parent_issuer",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.workload_trust_parent_issuer",
        unique: false,
        keys: &["parent_uid", "(spec->>'issuer')"],
        collations: &["", "pg_catalog.C"],
        predicate: "api_version='rise.dev/v1alpha1'ANDsplit_part(api_version,'/',1)='rise.dev'AND(kind=ANY(ARRAY['ControllerTrustPolicy','ServiceAccountTrustPolicy']))ANDdeletion_timestampISNULL",
    },
    ConcurrentIndexMigration {
        version: 20260719000008,
        name: "group_memberships_user_name",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.group_memberships_user_name",
        unique: false,
        keys: &["name"],
        collations: &["pg_catalog.C"],
        predicate: "api_version='rise.dev/v1alpha1'ANDsplit_part(api_version,'/',1)='rise.dev'ANDkind='GroupMembership'ANDdeletion_timestampISNULL",
    },
];

#[derive(sqlx::FromRow)]
struct ConcurrentIndexState {
    indisvalid: bool,
    indisready: bool,
    indislive: bool,
    indisunique: bool,
    table_schema: String,
    table_name: String,
    access_method: String,
    keys: Vec<String>,
    collations: Vec<String>,
    predicate: Option<String>,
}

fn normalize_catalog_expression(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .replace("::text", "")
}

fn index_matches_expected(
    expected: &ConcurrentIndexMigration,
    actual: &ConcurrentIndexState,
) -> bool {
    actual.indisvalid
        && actual.indisready
        && actual.indislive
        && actual.indisunique == expected.unique
        && actual.table_schema == "resource_store"
        && actual.table_name == "resources"
        && actual.access_method
            == if expected.name == "resources_owner_references_gin" {
                "gin"
            } else {
                "btree"
            }
        && actual.keys.len() == expected.keys.len()
        && actual.collations == expected.collations
        && actual
            .keys
            .iter()
            .zip(expected.keys)
            .all(|(actual, expected)| {
                normalize_catalog_expression(actual) == normalize_catalog_expression(expected)
            })
        && normalize_catalog_expression(actual.predicate.as_deref().unwrap_or_default())
            == normalize_catalog_expression(expected.predicate)
}

/// Recover the crash window between a no-transaction index statement and
/// SQLx's separate migration bookkeeping insert. Existing unrecorded indexes
/// are dropped and deterministically recreated by the migration. Recorded
/// migrations are fail-closed if their index was removed or is unusable.
async fn recover_concurrent_indexes(
    conn: &mut sqlx::PgConnection,
) -> Result<(), sqlx::migrate::MigrateError> {
    let migrations_table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('resource_store._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;

    for migration in CONCURRENT_INDEX_MIGRATIONS {
        let applied = if migrations_table_exists {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM resource_store._sqlx_migrations WHERE version = $1 AND success)",
            )
            .bind(migration.version)
            .fetch_one(&mut *conn)
            .await?
        } else {
            false
        };

        let state = sqlx::query_as::<_, ConcurrentIndexState>(
            r#"
            SELECT index.indisvalid,
                   index.indisready,
                   index.indislive,
                   index.indisunique,
                   table_namespace.nspname AS table_schema,
                   table_relation.relname AS table_name,
                   access_method.amname AS access_method,
                   ARRAY(
                       SELECT pg_get_indexdef(index.indexrelid, key_number, true)
                       FROM generate_series(1, index.indnkeyatts) AS key_number
                       ORDER BY key_number
                   ) AS keys,
                   ARRAY(
                       SELECT CASE
                                  WHEN key_collation.collation_oid = 0 THEN ''
                                  ELSE collation_namespace.nspname || '.' || catalog_collation.collname
                              END
                       FROM unnest(index.indcollation::oid[]) WITH ORDINALITY
                           AS key_collation(collation_oid, key_number)
                       LEFT JOIN pg_catalog.pg_collation catalog_collation
                         ON catalog_collation.oid = key_collation.collation_oid
                       LEFT JOIN pg_catalog.pg_namespace collation_namespace
                         ON collation_namespace.oid = catalog_collation.collnamespace
                       WHERE key_collation.key_number <= index.indnkeyatts
                       ORDER BY key_collation.key_number
                   ) AS collations,
                   pg_get_expr(index.indpred, index.indrelid, true) AS predicate
            FROM pg_catalog.pg_class relation
            JOIN pg_catalog.pg_namespace namespace
              ON namespace.oid = relation.relnamespace
            JOIN pg_catalog.pg_index index
              ON index.indexrelid = relation.oid
            JOIN pg_catalog.pg_class table_relation
              ON table_relation.oid = index.indrelid
            JOIN pg_catalog.pg_namespace table_namespace
              ON table_namespace.oid = table_relation.relnamespace
            JOIN pg_catalog.pg_am access_method
              ON access_method.oid = relation.relam
            WHERE namespace.nspname = 'resource_store'
              AND relation.relname = $1
            "#,
        )
        .bind(migration.name)
        .fetch_optional(&mut *conn)
        .await?;

        match (applied, state) {
            (false, Some(_)) => {
                // This is necessarily one of the static identifiers above.
                // No IF EXISTS: a concurrent/catalog mismatch must fail loudly.
                sqlx::query(migration.drop_sql).execute(&mut *conn).await?;
            }
            (true, Some(ref state)) if index_matches_expected(migration, state) => {}
            (false, None) => {}
            (true, state) => {
                return Err(sqlx::migrate::MigrateError::Execute(
                    sqlx::Error::Protocol(format!(
                        "recorded migration {} requires the expected valid index resource_store.{}; catalog state is {}",
                        migration.version, migration.name
                        , state.map(|state| format!("valid={}, ready={}, live={}, unique={}, table={}.{}, method={}, keys={:?}, collations={:?}, predicate={:?}", state.indisvalid, state.indisready, state.indislive, state.indisunique, state.table_schema, state.table_name, state.access_method, state.keys, state.collations, state.predicate)).unwrap_or_else(|| "missing".into())
                    )),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::CONCURRENT_INDEX_MIGRATIONS;

    #[test]
    fn concurrent_index_migrations_are_non_transactional_and_recoverable() {
        let migrator = sqlx::migrate!("./migrations");
        for expected in CONCURRENT_INDEX_MIGRATIONS {
            let migration = migrator
                .iter()
                .find(|migration| migration.version == expected.version)
                .expect("concurrent index migration exists");
            assert!(migration.no_tx);
            assert!(migration.sql.contains(expected.name));
            assert!(!migration.sql.contains("IF NOT EXISTS"));
            assert_eq!(expected.keys.len(), expected.collations.len());
        }
    }
}
