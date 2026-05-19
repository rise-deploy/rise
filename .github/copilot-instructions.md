# GitHub Copilot Instructions for Rise

This repository contains **Rise**, a Rust-based platform for deploying containerized applications with minimal configuration. Rise consists of a backend server (with embedded web UI) and a CLI tool, both built from a single consolidated Rust crate (`rise-deploy`).

## Project Overview

- **Language**: Rust 1.91+
- **Architecture**: Multi-process architecture with HTTP API server, deployment controllers, and CLI
- **Database**: PostgreSQL with SQLx (compile-time verified queries)
- **Authentication**: OAuth2/OIDC via Dex
- **Build System**: Cargo with feature flags (`cli`, `backend`)
- **Task Runner**: mise (see `mise.toml`)
- **Testing**: Unit tests, integration tests with test database

## Building and Running

### Prerequisites
- Rust 1.91+ (managed via mise)
- Docker and Docker Compose
- [mise](https://mise.jdx.dev/) task runner
- PostgreSQL (via Docker Compose)

### Quick Start
```bash
# Install tools and dependencies
mise install

# Start all services (postgres, dex, registry, backend)
mise backend:run

# Build CLI
cargo build --bin rise

# Build with specific features
cargo build --features backend  # Backend with all server-side capabilities
cargo build --all-features      # CLI + backend
```

### Key Commands

**Development:**
```bash
mise backend:run      # Start backend with all dependencies
mise backend:reload   # Reload backend (alias: mise br)
mise docs:serve       # Serve documentation at port 3001
```

**Database:**
```bash
mise db:migrate       # Run database migrations
mise db:nuke          # Drop and recreate database (USE WITH CAUTION)
sqlx migrate add <name>  # Create new migration
mise sqlx:prepare     # Update SQLX query cache (required after schema changes)
mise sqlx:check       # Verify SQLX queries are valid
```

**Linting and Testing:**
```bash
mise lint             # Run all linting checks (clippy, fmt, sqlx, helm)
cargo test            # Run tests (requires DATABASE_URL)
cargo test --all-features  # Test with all features enabled
```

**Format and Clippy:**
```bash
cargo fmt --all                                           # Format code
cargo fmt --all -- --check                                # Check formatting
cargo clippy --all-targets -- -D warnings                 # Lint with clippy
SQLX_OFFLINE=true cargo all-features clippy --all-targets -- -D warnings  # All feature combinations
```

## Code Structure

```
rise/
├── src/
│   ├── main.rs              # Binary entry point (CLI or server based on features)
│   ├── api/                 # Client-side API interface (feature: cli)
│   ├── build/               # Container image build orchestration (feature: cli)
│   ├── cli/                 # CLI command handlers (feature: cli)
│   ├── db/                  # Database access layer (SQLX helpers)
│   └── server/              # Backend server implementation (feature: backend)
│       ├── auth/            # OAuth2/OIDC authentication
│       ├── project/         # Project management
│       ├── team/            # Team management
│       ├── deployment/      # Kubernetes deployment controller
│       ├── ecr/             # AWS ECR integration
│       ├── encryption/      # AES-GCM and AWS KMS encryption
│       ├── oci/             # OCI registry client
│       └── frontend/        # Embedded web UI
├── migrations/              # Database migrations (SQLx)
├── docs/                    # User-facing documentation
├── helm/                    # Helm chart for Kubernetes deployment
├── config/                  # Backend configuration files
└── .sqlx/                   # SQLx offline query cache
```

## Coding Conventions

### Feature Flags
The crate uses Cargo feature flags for modular compilation:
- **`cli`** (default): CLI commands and client-side functionality
- **`backend`**: All server-side functionality including:
  - HTTP server, controllers, and backend logic
  - Kubernetes deployment controller
  - AWS ECR registry and KMS encryption
  - Snowflake OAuth provisioner

When adding code:
- CLI-only code goes in `src/cli/`, `src/api/`, or `src/build/` and should be feature-gated with `#[cfg(feature = "cli")]`
- Server-only code goes in `src/server/` and should be feature-gated with `#[cfg(feature = "backend")]`

### Database and SQLX Guidelines
- **All SQLX queries must be wrapped in helper functions** in the `src/db/` module
- **Never write SQLX queries directly** in server modules - always use `db::` helpers
- After schema changes, run `mise sqlx:prepare` to update the query cache
- The query cache (`.sqlx/`) must be committed for builds with `SQLX_OFFLINE=true` (used in CI)
- Integration tests should use a test database, not the development database

### Naming Conventions
- Use snake_case for functions, variables, and modules
- Use PascalCase for types, structs, and enums
- Database table names use snake_case
- CLI should accept **names** of things (project names, deployment timestamps), not UUIDs
- UUIDs in database tables are for internal book-keeping only

### Error Handling
- Use `anyhow::Result` for general error handling
- Use `thiserror` for custom error types when needed
- When logging errors without further handling, include context: `tracing::error!("Failed to do X: {:?}", error)`
- Return meaningful error messages to users via the CLI

### Authentication and Authorization
- Admin users should have full access to all operations
- When implementing new API endpoints, ensure admin users bypass regular permission checks
- Use JWT tokens for API authentication
- OAuth2 PKCE flow for CLI authentication (browser-based)

### Axum Routes
- Axum capture groups use the format `{capture}` (not `:capture`)
- Example: `/api/v1/projects/{project_name}`

### Code Style
- Follow standard Rust conventions (enforced by `rustfmt` and `clippy`)
- Don't add comments unless they match existing style or explain complex logic
- Use existing libraries when possible, avoid adding new dependencies unnecessarily
- Keep documentation lean - code should be readable, but add context where helpful

### Git and Commits
- Make small, focused commits with descriptive messages
- Don't commit the `.claude` directory
- Don't commit build artifacts or dependencies (e.g., `target/`, `node_modules/`)
- Ensure `.gitignore` is properly configured

## Testing

### Running Tests
```bash
# Start PostgreSQL
docker compose up -d postgres

# Set DATABASE_URL (or use direnv)
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/rise_test"

# Run migrations
sqlx migrate run

# Run tests
cargo test --all-features
```

### Writing Tests
- Use the test database (`rise_test`) for integration tests
- Clean up test data after each test (truncate tables or use transactions)
- Mock JWT tokens for testing protected endpoints
- Test both success and error responses
- Follow existing test patterns in `tests/` directory

### Test Accounts
**Admin user:**
- Email: `admin@example.com`
- Password: `password`

**Developer user:**
- Email: `dev@example.com`
- Password: `password`

**End user:**
- Email: `user@example.com`
- Password: `password`

## Documentation

- User-facing documentation is in `/docs/user` (built with Astro Starlight)
- Keep documentation updated when making changes
- Don't be overly verbose - focus on context and examples
- Bundled user docs live under `docs/user/src/content/docs`
- Engineering docs live under `docs/engineering/src/content/docs`

## Common Tasks

### Adding a New API Endpoint
1. Define route handler in appropriate `src/server/` module
2. Add route to Axum router in `src/server/mod.rs`
3. Implement authorization checks (ensure admin bypass)
4. Add database helpers in `src/db/` if needed
5. Update `mise sqlx:prepare` if queries changed
6. Add tests in `tests/` directory
7. Update API documentation

### Adding a New CLI Command
1. Add command handler in `src/cli/` module
2. Define clap command structure
3. Call backend API via `src/api/` client
4. Provide clear user feedback and error messages
5. Update CLI documentation in `docs/cli.md`

### Modifying Database Schema
1. Create migration: `sqlx migrate add <name>`
2. Write SQL in `migrations/<timestamp>_<name>.sql`
3. Run migration: `sqlx migrate run`
4. Update database helper functions in `src/db/`
5. Update SQLX cache: `mise sqlx:prepare`
6. Commit both migration and `.sqlx/` changes

### Adding a New Feature Flag
1. Add feature to `Cargo.toml` `[features]` section
2. Add optional dependencies as needed
3. Gate code with `#[cfg(feature = "feature-name")]`
4. Update CI workflow (`.github/workflows/ci.yml`) if needed
5. Update documentation

## CI/CD

The project uses GitHub Actions for CI:
- **Format check**: `cargo fmt --all -- --check`
- **Clippy**: All feature combinations with `cargo-all-features`
- **Tests**: With PostgreSQL service container
- **SQLX check**: Verify offline query cache is up-to-date
- **Build**: All targets with all feature combinations
- **Helm lint**: Validate Helm charts
- **Docker build**: Build and push images to GHCR
- **Documentation**: Build and deploy to GitHub Pages
- **Publish**: Publish to crates.io on version tags

All CI checks must pass before merging.

## Security Considerations

- Never commit secrets or credentials to the repository
- Use environment variables or encrypted storage for sensitive data
- Validate all user inputs
- Use prepared statements (SQLX) to prevent SQL injection
- Test authorization checks thoroughly
- Follow Rust security best practices

## Additional Resources

- **Architecture**: See `CLAUDE.md` for detailed architecture and implementation status
- **Documentation**: See `/docs` for comprehensive user guides
- **Troubleshooting**: See `docs/troubleshooting.md`
- **Production Deployment**: See `docs/production.md`

## Common Pitfalls to Avoid

- ❌ Writing SQLX queries outside of `src/db/` helpers
- ❌ Forgetting to run `mise sqlx:prepare` after schema changes
- ❌ Not testing with different feature flag combinations
- ❌ Adding dependencies without considering feature flags
- ❌ Forgetting to gate code with appropriate `#[cfg(feature = "...")]`
- ❌ Not handling admin user permission bypass in new endpoints
- ❌ Using UUIDs instead of names in CLI commands
- ❌ Committing without running lints and tests first

## Questions or Issues?

For detailed implementation context, refer to:
- `CLAUDE.md` - Implementation roadmap and guidelines
- `README.md` - Project overview and quick start
- `/docs` - Comprehensive documentation
- GitHub Issues - Track known issues and feature requests
