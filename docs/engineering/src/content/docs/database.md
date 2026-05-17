---
title: "Database"
---

# Database

Rise uses PostgreSQL for data storage with SQLX for compile-time verified SQL queries and migrations.

## Overview

Schema: Projects, Teams, Deployments, Service Accounts, Users

## Schema Management

Rise uses SQLX migrations for database schema versioning.

### Migrations Directory

Migrations in `./migrations/` (project root) with timestamp-based names.

### Creating Migrations

```bash
sqlx migrate add <description>
```

Creates `migrations/<timestamp>_<description>.sql`. Edit and add SQL.

### Running Migrations

**Development**: `mise db:migrate` (auto-run by `mise backend:run`)

**Production**: `sqlx migrate run`

### Migration Best Practices

1. Test on production copy first
2. Use `CREATE INDEX CONCURRENTLY` in PostgreSQL
3. Avoid blocking operations on large tables
4. Test rollback procedures

## SQLX Compile-Time Verification

`.sqlx/` directory contains query metadata for offline builds.

```bash
cargo sqlx prepare              # Generate metadata
cargo sqlx prepare --check      # Verify cache
```

Regenerate after migrations or SQL query changes.

### Writing Queries

Use `sqlx::query!` macro for compile-time verification (syntax, types, columns).

## Database Access

### Development

Connect to the local PostgreSQL database:

```bash
# Using psql
docker-compose exec postgres psql -U rise -d rise

# Or with connection string
psql postgres://rise:rise123@localhost:5432/rise
```

**Common queries**:

```sql
-- List all projects
SELECT * FROM projects;

-- Show deployment status
SELECT name, status, created_at FROM deployments ORDER BY created_at DESC LIMIT 10;

-- Count users
SELECT COUNT(*) FROM users;

-- Show team membership
SELECT t.name, u.email
FROM teams t
JOIN team_members tm ON t.id = tm.team_id
JOIN users u ON tm.user_id = u.id;
```

### Production

**Use read-only access for debugging**:

```bash
# Connect with read-only user
psql postgres://rise_readonly:password@rds-endpoint:5432/rise

# Limit query results
\set LIMIT 100
SELECT * FROM projects LIMIT :LIMIT;
```

**Never run write queries directly** on production. Use migrations instead.

## Resetting the Database

### Development

Completely reset the development database:

```bash
# Remove database volume
docker-compose down -v

# Start fresh
mise backend:run
```

This deletes all data and re-runs migrations.

### Soft Reset (Keep Schema)

Delete data without removing the schema:

```bash
# Connect to database
psql postgres://rise:rise123@localhost:5432/rise

# Truncate tables (preserves schema)
TRUNCATE deployments, projects, teams, team_members, users, service_accounts RESTART IDENTITY CASCADE;
```

## Common Patterns

### Transactions

Use transactions for multi-step operations:

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

sqlx::query!(
    "INSERT INTO audit_log (action, user_id) VALUES ($1, $2)",
    "create_project",
    user_id
)
.execute(&mut *tx)
.await?;

tx.commit().await?;
```

### Optional Fields

Handle NULL columns:

```rust
let deployment = sqlx::query!(
    r#"
    SELECT id, name, expires_at
    FROM deployments
    WHERE id = $1
    "#,
    deployment_id
)
.fetch_one(&pool)
.await?;

// expires_at is Option<DateTime<Utc>>
if let Some(expiry) = deployment.expires_at {
    println!("Expires at: {}", expiry);
}
```

### Custom Types

Use Postgres ENUM types:

```sql
CREATE TYPE visibility AS ENUM ('public', 'private');

ALTER TABLE projects ADD COLUMN visibility visibility NOT NULL DEFAULT 'public';
```

In Rust:

```rust
#[derive(Debug, sqlx::Type)]
#[sqlx(type_name = "visibility", rename_all = "lowercase")]
enum Visibility {
    Public,
    Private,
}
```

## Performance Considerations

### Indexes

Create indexes for frequently queried columns:

```sql
-- Lookups by owner
CREATE INDEX idx_projects_owner ON projects(owner_type, owner_id);

-- Deployment status queries
CREATE INDEX idx_deployments_status ON deployments(status) WHERE status != 'stopped';

-- Expiration cleanup
CREATE INDEX idx_deployments_expires_at ON deployments(expires_at) WHERE expires_at IS NOT NULL;
```

### Connection Pooling

Configure connection pool size in `config/production.toml` based on load and database limits.

### Query Optimization

Use `EXPLAIN ANALYZE` to optimize slow queries:

```sql
EXPLAIN ANALYZE
SELECT * FROM deployments
WHERE project_id = 123 AND status = 'running'
ORDER BY created_at DESC;
```

## Troubleshooting

### "Migrations have not been run"

**Problem**: Backend can't start because migrations are pending.

**Solution**:
```bash
mise db:migrate
```

### "SQLX cache is out of date"

**Problem**: Query metadata doesn't match actual database schema.

**Solution**:
```bash
cargo sqlx prepare
```

### "Connection refused"

**Problem**: Can't connect to PostgreSQL.

**Solution**:
```bash
# Check if PostgreSQL is running
docker-compose ps postgres

# Check logs
docker-compose logs postgres

# Restart
docker-compose restart postgres
```

### Deadlocks

**Problem**: Transactions blocking each other.

**Solution**:
- Keep transactions short
- Always acquire locks in the same order
- Use `SELECT ... FOR UPDATE NOWAIT` to fail fast

## Next Steps

- **Learn about local development**: See [Local Development](development.md)
- **Production database setup**: See [Production Setup](production.md)
