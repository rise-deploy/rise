Hi Gemini, my all time best software engineer in the world! I would like you to build a Rust-based project that is composed of a backend and
an accompagnying CLI that can be used to deploy very simple apps based on container images to a container runtime, of which our first
supported runtime will be Kubernetes but one could also imagine other runtimes such AWS Lambda or ECS, DO Apps, Docker, etc.

The idea is that the CLI makes it extremely easy to deploy such apps from the most minimal of configurations, for example by building
container images using buildpacks or nixpacks, but also supporting Dockerfiles, and possibly other methods.

For now we'll except that the container image will be built locally as part of the CLI call, but a future extension could be that we pass
through the details to a Buildkit daemon to be used when building the container image.

Container images are pushed to an internal container registry through temporary credentials that are passed to the frontend from the
backend.

The CLI allows creating and managing "projects" which represent an app that can be published. A project has a name, and the name defines how
the app is accessible under the common domain name for it (e.g. https://my-project.rise.dev). Users need to authenticate with the backend to
get access to manage projects. The backend must support local authentication (most useful for development) and OIDC (either login via OAuth2
and/or accepting JWT from a set of configured trusted issuers).

An example interaction with the `rise` CLI that a user might perform might be:

  $ rise login
  Please login to rise at https://rise.dev/oauth/login?code=1234-abcd
  Login successful! Welcome back, Niklas!

  $ rise p c secret-app --visibility private --owner team:devopsy
  Team 'devopsy' does not exist or you do not have permission to create projects for it. Did you mean 'devops'?

  $ rise p c secret-app --visibility private --owner team:devops
  Created project 'secret-app' with private visibility owned by team:devops.

  $ rise p ls
  PROJECT         STATUS        URL                             VISIBILITY    OWNER
  my-first-app    running       https://my-first-app.rise.dev   public        user:niklas
  secret-app      stopped       https://secret-app.rise.dev     private       team:devops

  $ cat .rise.toml
  project = "secret-app"

  [build]
  backend = "buildpacks"

  $ rise d c secret-app
  Building container image 'registry.rise.dev/secret-app:latest' using buildpacks...
  Pushing container image to registry.rise.dev...
  Deploying 'secret-app' ...
  Deployment successful! Your app is now running at https://secret-app.rise.dev

The backend and CLI should be designed with extensibility in mind, allowing for future support of additional container
runtimes, build methods, and authentication mechanisms. The CLI should provide clear and concise feedback to the user
at each step of the process, ensuring a smooth and user-friendly experience.

Let's outline the architecture and components needed for this Rust-based project, including both the backend and CLI.

## Architecture Overview

**Note**: The project is a Cargo **workspace**. The primary crate `rise-deploy` produces the `rise` binary with both CLI and server capabilities enabled via feature flags. A few focused, backend-only support crates live under `crates/` and are depended on as optional, `backend`-feature-gated path deps: `rise-resource-api` / `rise-resource-store` (generic resource API), and `rise-backend-auth` (pure-core token signing, verification, and matching — the single home for auth-token logic; see `AUTH_TOKEN_EXCHANGE_PLAN.md`).

### Crate Structure (`rise-deploy`)

The codebase is organized into functional modules:

- **`src/db/`**: Database access layer (PostgreSQL via SQLX) - shared by server modules
- **`src/server/`**: Backend server implementation with feature-gated modules:
   - **Authentication Module** (`auth/`): OAuth2/OIDC with Dex, JWT validation
   - **Project Management** (`project/`): Project CRUD and lifecycle management
   - **Team Management** (`team/`): Team and membership management
   - **Service Accounts** (`service_accounts/`): CI/CD service accounts (inbound OIDC federation into Rise)
   - **Workload Identity Tokens** (`workload_tokens/`): Token-exchange endpoint issuing Rise-signed workload JWTs to deployed apps
   - **Container Registry** (`registry/`): Temporary credentials for ECR registries
   - **Deployment Module** (`deployment/`): Kubernetes controller for deployments
   - **ECR Integration** (`ecr/`): AWS ECR repository management
   - **Encryption** (`encryption/`): Local AES-GCM and AWS KMS providers
   - **OCI Client** (`oci/`): OCI registry interaction
   - **Frontend** (`frontend/`): Static web UI assets
   - **API Layer**: RESTful endpoints via Axum
- **`src/cli/`**: CLI command handlers (feature: `cli`)
   - Authentication, project, team, deployment, environment variable commands
- **`src/build/`**: Container image build orchestration (feature: `cli`)
   - Support for Docker, Pack (buildpacks), and Railpack backends
   - BuildKit daemon management, SSL certificate handling
- **`src/api/`**: Client-side API interface for server communication (feature: `cli`)

### Feature Flags

The crate uses Cargo features for modular compilation:

- **`cli`** (default): CLI commands and client-side functionality
- **`backend`**: All server-side functionality including:
  - HTTP server, controllers, and backend logic
  - Kubernetes deployment controller
  - AWS ECR registry and KMS encryption
  - Snowflake OAuth provisioner

Examples:
```bash
cargo build                    # CLI-only build (smallest binary)
cargo build --features backend # Server with all backend capabilities
cargo build --all-features     # Full build with CLI + backend
```

## Implementation Steps

**Project Structure**: A Cargo workspace whose primary crate `rise-deploy` carries both CLI and backend (feature-gated). Focused, backend-only support crates live under `crates/` — `rise-resource-api` / `rise-resource-store` and the pure-core `rise-backend-auth` (the single home for auth-token signing, verification, and matching).

### Completed Implementation

1. **Core Infrastructure** ✅
   - [x] Single consolidated crate with feature flags (`cli`, `backend`)
   - [x] PostgreSQL database with SQLX (compile-time verified queries and migrations)
   - [x] Dex OAuth2/OIDC integration for authentication
   - [x] Docker Compose setup for local development (PostgreSQL, Dex, Registry)

2. **Server Implementation** (`--features backend`) ✅
   - [x] Axum-based HTTP API with RESTful endpoints
   - [x] Authentication: OAuth2/OIDC with Dex, JWT validation, PKCE flow
   - [x] Project management: CRUD operations, ownership, visibility
   - [x] Team management: Team creation, membership, role-based access
   - [x] Deployment controller:
     - [x] Kubernetes controller - K8s deployments with Ingress
   - [x] Container registry integration:
     - [x] AWS ECR provider with repository lifecycle management
   - [x] Encryption providers: Local AES-GCM and AWS KMS
   - [x] OCI client for image digest resolution
   - [x] Frontend static web UI
   - [x] Extensions system:
     - [x] Multiple instances per extension type
     - [x] Generic OAuth 2.0 provider for end-user authentication
       - [x] Fragment flow (default) - tokens in URL fragment for SPAs
       - [x] Exchange token flow - secure backend exchange for server-rendered apps
       - [x] Session-based token caching with automatic refresh
       - [x] Encrypted token storage (AES-GCM/KMS)
       - [x] Support for any OAuth 2.0 provider (Snowflake, Google, GitHub, custom SSO)
       - [x] Client secret stored as encrypted environment variables
     - [x] AWS RDS extension for database provisioning
     - [x] Snowflake OAuth provisioner for Snowflake security integrations

3. **CLI Implementation** (`--features cli`, default) ✅
   - [x] OAuth2 authorization code flow with PKCE (browser-based, default)
   - [x] Project commands: `create`, `list`, `show`, `update`, `delete`
   - [x] Team commands: `create`, `list`, `show`, `update`, `delete`
   - [x] Deployment commands: `create`, `list`, `show`, `rollback`, `stop`
   - [x] Environment variable management
   - [x] Service account (workload identity) management

4. **Build System** (`--features cli`) ✅
   - [x] Docker backend: Standard Dockerfile builds
   - [x] Pack backend: Cloud Native Buildpacks integration
   - [x] Railpack backend: Schema.org Railpacks with BuildKit/Buildx
   - [x] Automatic build method detection
   - [x] BuildKit daemon management with SSL certificate handling
   - [x] `rise build` command for local image builds without deployment
   - [x] `rise run` command for local development (build and run with docker/podman)
   - [x] Pre-built image deployment support (`--image` flag)
   - [x] Deployment following with auto-refresh and timeout support

## User-Facing Documentation

For user-facing documentation, see the [`/docs`](./docs) directory. Key topics include:
- Build backends (Docker, Pack, Railpack): [docs/user-guide/builds.md](docs/user-guide/builds.md)
- SSL & proxy configuration: [docs/user-guide/ssl-proxy.md](docs/user-guide/ssl-proxy.md)
- Architecture and process design: [docs/development.md](docs/development.md)
- Configuration: [docs/configuration.md](docs/configuration.md)
- OAuth extension (end-user authentication): [docs/user-guide/oauth.md](docs/user-guide/oauth.md)

## Git Branching

- The default development branch is `develop`. PRs for feature work should target `develop`, not `main`.
- Always target the branch your feature branch was created from when opening a PR.

## Guidelines

- Build features in small increments with frequent commits. Use Git history as a reference for what was done and why.
- Keep this document updated as the project evolves.
- Write clean, maintainable code following Rust best practices. Prioritize user experience in the CLI.
- Don't commit the .claude directory
- Axum capture groups are formatted as `{capture}`
- Keep the documentation updated. Don't be overly verbose when documenting the project. People can read the code, but things that are not obvious or help getting started and context are usually helpful in documentation, as well as well-placed and lean examples.
- When removing a feature, do a comprehensive check on the codebase to ensure any remaining references to that feature are removed or updated. This includes documentation files/READMEs, config files, code comments, etc.
- Don't reference previous versions of the code in comments, docs, or commit-independent artifacts (e.g. "the previous design did X", "vs the old tick counter", "this used to be Y"). Comments must describe what the code does *now* and why — a reader has no access to the version you're contrasting against, and such notes rot. Git history is the place for that context. (Referring to runtime/domain concepts like "the previous leader replica" is fine — that's not code history.)
- The CLI should first and foremost always accept the names of things (e.g. project names, or project names + deployment timestamp). The UUIDs in our tables are only for internal book-keeping.
- Admin users (`auth.admin_users`) bypass the regular permission checks on the typed APIs (projects, teams, deployments, etc.) — they have full access there without passing ownership/membership checks. This does **not** extend to the generic resource API (`/api/v1/resources`), which is operator-gated (`auth.operator_users`): admins are not operators and do not bypass its checks. Granting admins access to the resource API is intentionally deferred (see `MULTI_TENANCY_PLAN.md`).
- Any SQLX queries are to be wrapped by helper functions in the `rise_deploy::db` crate. No SQLX queries outside of this crate are allowed.
- When we log errors and don't handle them further, we should include a sensible amount of information about the error. Often logging the error with `{:?}` is good enough.
- When capturing screenshots, the playwright tool will successfully install the driver even if you might think its install step failed. Always use minimum 1280px width and 800px height for the browser.
- Never use `// @ts-nocheck` (or `// @ts-ignore` / `// @ts-expect-error`) in frontend code. Existing files that still carry the directive are legacy; new files must not add it, and edits to legacy files should remove it where feasible. If TypeScript flags something, fix the types — don't suppress the diagnostic.

### Tag/Badge Design Language

The frontend exposes a small set of reusable tag-like components from `frontend/src/components/r-ui.tsx`. Prefer these over inline Tailwind / ad-hoc spans for any tag-like UI.

| Component | Style | Purpose |
|---|---|---|
| **`Status`** (from `r-ui`) | Dot + label | Deployment/project lifecycle status (Running, Failed, Stopped, …) |
| **`Pill`** (from `r-ui`) | Bordered, colored bg | Accent labels via `kind="accent"`, env labels via `kind="env-*"` |
| **`BasePill`** (from `r-ui`) | Icon + name pill | Generic two-cell pill — building block for `EnvPill` / `GroupPill` |
| **`EnvPill`** (from `r-ui`) | Color dot + env name | Environment chip; pairs with `EnvironmentColorDot` |
| **`GroupPill`** (from `r-ui`) | Layer glyph + group name | Group chip |
| **`EnvironmentColorDot`** (from `r-ui`) | Tinted layer glyph | Placed next to environment names to indicate color |

### Before Commit & Push

**MANDATORY**: You MUST run `cargo fmt --all` before every commit. Always. No exceptions. CI will reject unformatted code. Run it, stage any formatting changes, then commit.

**Always run** (fast, catches most issues):

```bash
cargo fmt --all                # Format code — MUST run before every commit
cargo clippy --all-features --all-targets -- -D warnings  # Lint (uses cached build artifacts)
```

**Run selectively** based on what changed:

| What changed | Command | Why |
|---|---|---|
| Any `.rs` file | `cargo test --workspace --all-features` | Unit tests (requires `mise run db:migrate` once); `--workspace` ensures crates such as `rise-backend-auth` are included |
| SQLX queries (`sqlx::query!` etc.) | `mise run sqlx:prepare` | Regenerate offline query cache (commit the result) |
| Server settings structs (`src/server/settings.rs`) | `mise run config:schema:generate` | Regenerate `docs/engineering/public/schemas/backend-settings.schema.json` (commit the result) |
| `src/rise_toml.rs` structs | `mise run rise-toml:schema:generate` | Regenerate `docs/user/public/schemas/rise-toml-v1.schema.json` (commit the result) |
| CRD structs (`src/server/deployment/crd.rs`) | `mise run crd:generate` | Regenerate `helm/rise/crds/riseproject-crd.yaml` (commit the result) |
| Helm chart (`helm/rise/`) | `helm lint helm/rise` | Validate chart templates |

**Full CI-equivalent check** (slower, runs everything):

```bash
mise run lint                  # cargo check + clippy + fmt check + sqlx check + helm lint
mise run config:schema:check        # Verify backend config schema is up to date
mise run rise-toml:schema:check    # Verify rise.toml schema is up to date
mise run crd:check                 # Verify CRD YAML matches Rust definition
cargo test --workspace --all-features  # Unit tests (all crates)
```

The `mise run lint` task runs: `cargo all-features check`, `cargo all-features clippy -- -D warnings`, `cargo fmt --check`, `mise sqlx:check`, and `helm lint helm/rise`.

## Future Enhancements

### Ingress Authentication (Kubernetes Controller)

The project `visibility` field (Public/Private) is currently stored but not enforced at the ingress level. This field is intended for ingress-level authentication:

- **Public projects**: The ingress will serve the application without requiring authentication
- **Private projects**: The ingress will require user authentication AND verify project access authorization before serving the application

**Current State**: The visibility field is stored in the database and returned via the API, but does NOT affect:
- API authorization (all projects require ownership/team membership to access via API)
- Ingress routing (authentication not yet configured in ingress annotations)

**Implementation Plan**:
- The Kubernetes controller will configure ingress resources based on the visibility field
- Public projects will have standard ingress rules
- Private projects will have OAuth2 proxy or similar authentication middleware configured in the ingress
- The authentication layer will validate both user identity AND project access permissions before proxying requests to the application
