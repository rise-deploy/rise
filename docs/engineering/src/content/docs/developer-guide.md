---
title: "Developer Guide"
---

The Developer Guide is for contributors working on the Rise codebase.

Use this section if you are:

- Developing backend or frontend features
- Running tests and validating local changes
- Working on migrations or internal architecture updates

For normal product usage, use the User Guide.

## SQLX

Rise uses SQLX for compile-time verified SQL queries. Query metadata lives in the `.sqlx/` directory.

```bash
cargo sqlx prepare              # Regenerate metadata after schema/query changes
cargo sqlx prepare --check      # Verify cache matches current schema (run in CI)
```

Regenerate after adding migrations or changing SQL queries. See the [CLAUDE.md](../../../../CLAUDE.md) for when to run this.

### DATABASE_URL at Compile Time

`DATABASE_URL` must be set when running `cargo sqlx prepare` (or any `cargo build` that invokes `sqlx::query!` macros without an up-to-date `.sqlx/` cache). It must point to a running PostgreSQL instance with all migrations applied so SQLX can verify queries against the schema.

```bash
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/rise"
sqlx migrate run          # ensure migrations are applied
cargo sqlx prepare        # regenerate the .sqlx/ cache
```

At **runtime** the database URL comes from `settings.database.url` (resolved from the config file, with `DATABASE_URL` as a fallback). See [Configuration](./configuration.md) for the full precedence rules.

### Writing Queries

Use the `sqlx::query!` macro for compile-time verification of syntax, types, and columns.

### Transactions

```rust
let mut tx = pool.begin().await?;

sqlx::query!(
    "INSERT INTO projects (name, owner_type, owner_id) VALUES ($1, $2, $3)",
    name,
    "user",
    user_id
)
.execute(&mut *tx)
.await?;

tx.commit().await?;
```

### Optional Fields

NULL columns map to `Option<T>`:

```rust
let deployment = sqlx::query!(
    "SELECT id, name, expires_at FROM deployments WHERE id = $1",
    deployment_id
)
.fetch_one(&pool)
.await?;

if let Some(expiry) = deployment.expires_at {
    println!("Expires at: {}", expiry);
}
```

### Custom Types

Define Postgres ENUM types in migrations, then derive `sqlx::Type` in Rust:

```sql
CREATE TYPE visibility AS ENUM ('public', 'private');
```

```rust
#[derive(Debug, sqlx::Type)]
#[sqlx(type_name = "visibility", rename_all = "lowercase")]
enum Visibility {
    Public,
    Private,
}
```
