---
title: "Testing"
---

# Testing

Guidelines for testing Rise components.

## Overview

Rise uses multiple testing strategies:

- **Unit tests**: Test individual functions and modules
- **Integration tests**: Test API endpoints and database interactions
- **End-to-end tests**: Test full workflows via CLI

## Integration Tests

Integration tests are in the `tests/` directory and test API endpoints with a real database.

### Setup

Integration tests use a test database:

```rust
// tests/common.rs
pub async fn setup_test_db() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://rise:rise123@localhost:5432/rise_test".to_string());

    let pool = PgPool::connect(&database_url).await.unwrap();

    // Run migrations
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .unwrap();

    pool
}

pub async fn cleanup_test_db(pool: &PgPool) {
    sqlx::query("TRUNCATE projects, deployments, teams, users CASCADE")
        .execute(pool)
        .await
        .unwrap();
}
```

### Example Integration Test

```rust
// tests/projects_api.rs
use rise_backend::app;
use axum::http::StatusCode;

#[tokio::test]
async fn test_create_project() {
    let pool = common::setup_test_db().await;
    let app = app(pool.clone()).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/projects")
                .header("content-type", "application/json")
                .body(r#"{"name": "test-app", "visibility": "public"}"#)
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    common::cleanup_test_db(&pool).await;
}
```

### Best Practices

- **Use test database**: Never test against production or development databases
- **Clean up after tests**: Truncate tables or use transactions
- **Test authentication**: Mock JWT tokens for protected endpoints
- **Test error responses**: Verify 400, 401, 404 responses

## End-to-End Tests

Test full workflows using the CLI:

```bash
#!/bin/bash
# tests/e2e/deploy_workflow.sh

# Login
rise login --email dev@example.com --password password

# Create project
rise project create e2e-test --visibility public

# Deploy
rise deployment create e2e-test --image nginx:latest

# Verify deployment
STATUS=$(rise deployment show e2e-test:latest --format json | jq -r '.status')
if [ "$STATUS" != "running" ]; then
  echo "Deployment failed"
  exit 1
fi

# Cleanup
rise project delete e2e-test
```

## Test Data

### Development Accounts
**Admin user:**
- Email: `admin@example.com`
- Password: `password`

**Developer user:**
- Email: `dev@example.com`
- Password: `password`

**End user:**
- Email: `user@example.com`
- Password: `password`

### Creating Test Projects

```bash
# Create test projects
rise project create test-app-1 --visibility public
rise project create test-app-2 --visibility private
```

### Mock Data

For unit tests, create mock data:

```rust
fn mock_project() -> Project {
    Project {
        id: Uuid::new_v4(),
        name: "test-app".to_string(),
        owner_type: OwnerType::User,
        owner_id: Uuid::new_v4(),
        visibility: Visibility::Public,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}
```

## Troubleshooting

### "Database connection failed"

Ensure PostgreSQL is running:

```bash
docker-compose up -d postgres
```

Set `DATABASE_URL`:

```bash
export DATABASE_URL="postgres://rise:rise123@localhost:5432/rise_test"
```

### "Migration not found"

Run migrations before tests:

```bash
cd rise-backend
sqlx migrate run
```

### Tests are slow

Use `cargo test --release` for faster execution (but slower compilation).

Run specific tests instead of the full suite:

```bash
cargo test test_projects
```

## Test Coverage

### Measuring Coverage

Use `cargo-tarpaulin` for code coverage:

```bash
# Install tarpaulin
cargo install cargo-tarpaulin

# Run coverage
cargo tarpaulin --out Html --output-dir coverage
```

Open `coverage/index.html` to view results.

### Coverage Goals

- **Critical paths**: 90%+ coverage (authentication, deployments)
- **Utility functions**: 80%+ coverage
- **Overall**: 70%+ coverage

## Next Steps

- **Set up development environment**: See [Local Development](development.md)
- **Database testing**: See [Database](database.md)
