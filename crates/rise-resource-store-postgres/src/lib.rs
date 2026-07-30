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

/// Empty clone of `resource_store.resources` that every expected index
/// definition is replayed against. See [`ConcurrentIndexMigration::reference_sql`].
const DRIFT_REFERENCE_TABLE: &str = "rise_concurrent_index_reference";

struct ConcurrentIndexMigration {
    version: i64,
    name: &'static str,
    drop_sql: &'static str,
    reference_index: &'static str,
    /// The same index definition, targeting [`DRIFT_REFERENCE_TABLE`].
    ///
    /// Drift is judged by comparing the live catalog entry against this
    /// reference entry, so both sides are deparsed by the server that is
    /// actually running. PostgreSQL's expression pretty-printing is not a
    /// stable cross-version format — parenthesization, cast rendering, and
    /// `IN` normalization all change between major versions — so an expected
    /// definition must never be a deparsed literal baked into the binary. That
    /// would turn a routine PostgreSQL upgrade into a startup outage on an
    /// index that is perfectly correct.
    reference_sql: &'static str,
}

const CONCURRENT_INDEX_MIGRATIONS: &[ConcurrentIndexMigration] = &[
    ConcurrentIndexMigration {
        version: 20260719000001,
        name: "resources_owner_references_gin",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.resources_owner_references_gin",
        reference_index: "reference_resources_owner_references_gin",
        reference_sql: r#"
CREATE INDEX reference_resources_owner_references_gin
    ON pg_temp.rise_concurrent_index_reference
    USING GIN (owner_references jsonb_path_ops)
"#,
    },
    ConcurrentIndexMigration {
        version: 20260719000006,
        name: "user_identities_issuer_subject_unique",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.user_identities_issuer_subject_unique",
        reference_index: "reference_user_identities_issuer_subject_unique",
        reference_sql: r#"
CREATE UNIQUE INDEX reference_user_identities_issuer_subject_unique
    ON pg_temp.rise_concurrent_index_reference (
        ((spec->>'issuer') COLLATE "C"),
        ((spec->>'subject') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'UserIdentity'
      AND deletion_timestamp IS NULL
"#,
    },
    ConcurrentIndexMigration {
        version: 20260719000007,
        name: "workload_trust_parent_issuer",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.workload_trust_parent_issuer",
        reference_index: "reference_workload_trust_parent_issuer",
        reference_sql: r#"
CREATE INDEX reference_workload_trust_parent_issuer
    ON pg_temp.rise_concurrent_index_reference (
        parent_uid,
        ((spec->>'issuer') COLLATE "C")
    )
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind IN ('ControllerTrustPolicy', 'ServiceAccountTrustPolicy')
      AND deletion_timestamp IS NULL
"#,
    },
    ConcurrentIndexMigration {
        version: 20260719000008,
        name: "group_memberships_user_name",
        drop_sql: "DROP INDEX CONCURRENTLY resource_store.group_memberships_user_name",
        reference_index: "reference_group_memberships_user_name",
        reference_sql: r#"
CREATE INDEX reference_group_memberships_user_name
    ON pg_temp.rise_concurrent_index_reference ((name COLLATE "C"))
    WHERE split_part(api_version, '/', 1) = 'rise.dev'
      AND kind = 'GroupMembership'
      AND deletion_timestamp IS NULL
"#,
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
    operator_classes: Vec<String>,
    predicate: Option<String>,
}

impl ConcurrentIndexState {
    fn describe(&self) -> String {
        format!(
            "valid={}, ready={}, live={}, unique={}, table={}.{}, method={}, keys={:?}, collations={:?}, operator_classes={:?}, predicate={:?}",
            self.indisvalid,
            self.indisready,
            self.indislive,
            self.indisunique,
            self.table_schema,
            self.table_name,
            self.access_method,
            self.keys,
            self.collations,
            self.operator_classes,
            self.predicate
        )
    }
}

/// Compare a live index against the reference index built from the same
/// definition on this server. Every deparsed value on both sides came from the
/// same PostgreSQL instance, so plain equality is exact without needing to
/// normalize a format PostgreSQL does not promise to keep stable.
fn index_matches_reference(
    reference: &ConcurrentIndexState,
    actual: &ConcurrentIndexState,
) -> bool {
    actual.indisvalid
        && actual.indisready
        && actual.indislive
        && actual.table_schema == "resource_store"
        && actual.table_name == "resources"
        && actual.indisunique == reference.indisunique
        && actual.access_method == reference.access_method
        && actual.keys == reference.keys
        && actual.collations == reference.collations
        && actual.operator_classes == reference.operator_classes
        && actual.predicate == reference.predicate
}

/// Read one index's catalog definition, or `None` when it does not exist.
async fn fetch_index_state(
    conn: &mut sqlx::PgConnection,
    schema: &str,
    name: &str,
) -> Result<Option<ConcurrentIndexState>, sqlx::migrate::MigrateError> {
    sqlx::query_as::<_, ConcurrentIndexState>(
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
               ARRAY(
                   SELECT operator_class_namespace.nspname || '.' || operator_class.opcname
                   FROM unnest(index.indclass::oid[]) WITH ORDINALITY
                       AS key_class(class_oid, key_number)
                   JOIN pg_catalog.pg_opclass operator_class
                     ON operator_class.oid = key_class.class_oid
                   JOIN pg_catalog.pg_namespace operator_class_namespace
                     ON operator_class_namespace.oid = operator_class.opcnamespace
                   WHERE key_class.key_number <= index.indnkeyatts
                   ORDER BY key_class.key_number
               ) AS operator_classes,
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
        WHERE namespace.nspname = $1
          AND relation.relname = $2
        "#,
    )
    .bind(schema)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Into::into)
}

/// Materialize every expected index against an empty clone of the resources
/// table and return the temporary schema holding them. The clone is empty, so
/// each index build is effectively free.
async fn build_drift_reference(
    conn: &mut sqlx::PgConnection,
) -> Result<String, sqlx::migrate::MigrateError> {
    use sqlx::Executor;

    conn.execute(
        format!("CREATE TEMP TABLE {DRIFT_REFERENCE_TABLE} (LIKE resource_store.resources)")
            .as_str(),
    )
    .await?;
    for migration in CONCURRENT_INDEX_MIGRATIONS {
        conn.execute(migration.reference_sql).await?;
    }
    sqlx::query_scalar(
        "SELECT nspname FROM pg_catalog.pg_namespace WHERE oid = pg_my_temp_schema()",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(Into::into)
}

/// Recover the crash window between a no-transaction index statement and
/// SQLx's separate migration bookkeeping insert. Existing unrecorded indexes
/// are dropped and deterministically recreated by the migration. Recorded
/// migrations are fail-closed if their index was removed or is unusable.
async fn recover_concurrent_indexes(
    conn: &mut sqlx::PgConnection,
) -> Result<(), sqlx::migrate::MigrateError> {
    use sqlx::Executor;

    // A database without the resources table cannot hold any of these indexes,
    // and cannot be cloned for the drift reference either.
    let resources_table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('resource_store.resources') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !resources_table_exists {
        return Ok(());
    }
    let migrations_table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('resource_store._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;

    let reference_schema = build_drift_reference(&mut *conn).await?;

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

        let state = fetch_index_state(&mut *conn, "resource_store", migration.name).await?;

        match (applied, state) {
            (false, Some(_)) => {
                // This is necessarily one of the static identifiers above.
                // No IF EXISTS: a concurrent/catalog mismatch must fail loudly.
                sqlx::query(migration.drop_sql).execute(&mut *conn).await?;
            }
            (false, None) => {}
            (true, state) => {
                let reference =
                    fetch_index_state(&mut *conn, &reference_schema, migration.reference_index)
                        .await?
                        .ok_or_else(|| {
                            sqlx::migrate::MigrateError::Execute(sqlx::Error::Protocol(format!(
                                "drift reference index {} was not created",
                                migration.reference_index
                            )))
                        })?;
                if !state
                    .as_ref()
                    .is_some_and(|state| index_matches_reference(&reference, state))
                {
                    return Err(sqlx::migrate::MigrateError::Execute(
                        sqlx::Error::Protocol(format!(
                            "recorded migration {} requires the expected valid index resource_store.{}; catalog state is {}; reference state is {}",
                            migration.version,
                            migration.name,
                            state
                                .map(|state| state.describe())
                                .unwrap_or_else(|| "missing".into()),
                            reference.describe()
                        )),
                    ));
                }
            }
        }
    }

    conn.execute(format!("DROP TABLE pg_temp.{DRIFT_REFERENCE_TABLE}").as_str())
        .await?;
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::{CONCURRENT_INDEX_MIGRATIONS, DRIFT_REFERENCE_TABLE};

    /// Drop SQL line comments and collapse whitespace to one canonical line.
    fn canonical_statement(sql: &str) -> String {
        sql.lines()
            .filter(|line| !line.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Everything in an index statement after its target table: the key list,
    /// operator classes, and predicate. Only the index and table names differ
    /// between a migration and its drift reference, so these tails must be
    /// identical or the reference is no longer describing the real index.
    fn index_definition_tail(sql: &str) -> String {
        let canonical = canonical_statement(sql);
        let (_, after_on) = canonical
            .split_once(" ON ")
            .expect("index statement has an ON clause");
        let (_, tail) = after_on
            .split_once(' ')
            .expect("index statement has a definition after its table");
        tail.trim().trim_end_matches(';').trim().to_string()
    }

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
            assert!(expected.reference_sql.contains(DRIFT_REFERENCE_TABLE));
            assert!(expected.reference_sql.contains(expected.reference_index));
            assert_eq!(
                canonical_statement(&migration.sql).contains("UNIQUE INDEX"),
                canonical_statement(expected.reference_sql).contains("UNIQUE INDEX"),
                "uniqueness of {} and its drift reference disagree",
                expected.name
            );
            assert_eq!(
                index_definition_tail(&migration.sql),
                index_definition_tail(expected.reference_sql),
                "migration {} and its drift reference define different indexes",
                expected.version
            );
        }
    }
}
