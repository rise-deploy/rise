# Rise

Rise is a Rust project — a backend and an accompanying `rise` CLI — for deploying
apps packaged as container images to a container runtime. Kubernetes and Docker are
the supported runtimes today; the design deliberately leaves room for others (ECS,
Lambda, DO Apps, …).

The CLI deploys from the most minimal of configurations. It builds the container
image locally (Dockerfile, Cloud Native Buildpacks, or Railpack), pushes it to a
container registry using temporary credentials issued by the backend, and asks the
backend to deploy it. A future extension is to hand the build off to a BuildKit
daemon instead of building in-process.

A **project** is the unit of deployment. Its name determines how the app is reached
under the install's common domain (e.g. `https://my-project.rise.dev`). Users
authenticate against the backend to manage projects — via OAuth2/OIDC (Dex in
development), or by presenting JWTs from a configured set of trusted issuers.

A typical session:

```console
$ rise login
Please login to rise at https://rise.dev/oauth/login?code=1234-abcd
✓ Login successful!
  Token saved to: ~/.config/rise/config.json

$ rise p c secret-app --access-class private --owner team:devopsy
Team 'devopsy' does not exist or you do not have permission to create projects for it.

Did you mean one of these?
  - devops

$ rise p c secret-app --access-class private --owner team:devops
✓ Project 'secret-app' created successfully!
  ID: 0f3c…
  Status: Stopped
✓ Created rise.toml at ./rise.toml

$ rise p ls
NAME            STATUS     ACCESS CLASS   OWNER         ACTIVE DEPLOYMENT   URL
my-first-app    Running    public         user:niklas   Healthy             https://my-first-app.rise.dev
secret-app      Stopped    private        team:devops   -                   https://secret-app.rise.dev

$ cat rise.toml           # what `project create` writes
version = 1

[project]
name = "secret-app"

$ cat >> rise.toml <<'TOML'   # pin the build backend

[build]
backend = "pack"
TOML

$ rise d c secret-app
Building container image 'registry.rise.dev/secret-app:latest' using pack...
Pushing container image to registry.rise.dev...
Deploying 'secret-app' ...
Deployment successful! Your app is now running at https://secret-app.rise.dev
```

Backend and CLI are designed for extensibility — additional container runtimes,
build methods, and authentication mechanisms. The CLI gives clear, concise feedback
at each step.

## Architecture Overview

The project is a Cargo **workspace**. The primary crate `rise-deploy` produces the
`rise` binary with both CLI and server capabilities enabled via feature flags.
Focused support crates live under `crates/`:

| Crate | Purpose | Linkage |
|---|---|---|
| `rise-deployment-spec` | Shared deployment/project-config model — `AccessRequirement`, `RouteAccess`, request spec, validation | workspace dep (both CLI and backend) |
| `rise-backend-auth` | Pure-core token signing, verification, and claim matching — the single home for auth-token logic (see `ROADMAP.md` § 2, "Rise-issued authentication and token issuance") | `backend` |
| `rise-backend-core` | The deployment-backend contract seam: shared deployment models, the `DeploymentBackend` trait, registry/encryption provider traits, the pure `quantity`/`state_machine`/`runtime`/`url_builder`/`token_ttl`/`custom_domain` helpers, and the `DeploymentStore` trait — the database boundary implemented by `rise-deploy`'s `PgDeploymentStore` | `backend` |
| `rise-backend-docker` | The Docker deployment backend: `DockerBackend` + the in-process `DockerReconciler`, the first controller extracted onto the `rise-backend-core` seam. Re-exported under `crate::server::deployment::controller::docker`, so existing module paths keep resolving | `backend` |
| `rise-authz` | Authorization policy evaluation. `policy` is a hard Tier-0 boundary: pure functions and canonical values only — no store, database, HTTP, or product-resource dependencies | `backend` |
| `rise-resource-api` | Generic resource API contract (resource kinds, scopes, owner references, identity, policy types) | `backend` |
| `rise-resource-store-postgres` | PostgreSQL adapter for the resource API. Owns its own migrations in a `resource_store` schema and its own SQLX offline cache | `backend` |
| `rise-runtime-sync` | Postgres-backed cross-replica primitives: `GlobalLock`, `LeaderElection`, `GlobalSchedule`. Owns its own migrations in a `runtime_sync` schema and its own SQLX offline cache | `backend` |

The Kubernetes controller is still in-tree (`src/server/deployment/controller/kubernetes.rs`)
pending the same extraction onto the `rise-backend-core` seam that `rise-backend-docker`
received.

The **e2e harness** (`tests/e2e`) is a standalone workspace, `exclude`d from the root
one so production image builds never compile it. It is linted and tested separately —
see [Before Commit & Push](#before-commit--push).

### Crate Structure (`rise-deploy`)

The codebase is organized into functional modules:

- **`src/db/`**: Database access layer (PostgreSQL via SQLX) — shared by server modules
- **`src/server/`**: Backend server implementation (feature: `backend`)
   - **Authentication** (`auth/`): OAuth2/OIDC with Dex, JWT validation, ingress auth subrequests, admin/operator classification
   - **Project Management** (`project/`): Project CRUD and lifecycle management
   - **Team Management** (`team/`): Team and membership management
   - **Environments** (`environments/`): Named environments (production, staging, …)
   - **Environment Variables** (`env_vars/`): Plain and encrypted per-project variables
   - **Custom Domains** (`custom_domains/`): Custom domain registration, verification, TLS wiring
   - **Service Accounts** (`service_accounts/`): CI/CD service accounts (inbound OIDC federation into Rise)
   - **Workload Identity Tokens** (`workload_tokens/`): Token-exchange endpoint issuing Rise-signed workload JWTs to deployed apps
   - **Generic Resources** (`resources/`): The operator-gated generic resource API (`/api/v1/resources`) and its garbage collector
   - **Platform** (`platform/`): Platform/organization-level concerns
   - **Quickstart** (`quickstart/`): Catalog of ready-to-deploy templates (see `config/default.yaml`)
   - **Extensions** (`extensions/`): Extension registry and providers
   - **Container Registry** (`registry/`): Temporary registry credentials — ECR, GitLab, JFrog, and generic OCI basic-auth providers
   - **Deployment Module** (`deployment/`): Deployment models, CRD, webhook, logs, resource builder, and the Kubernetes controller (the Docker controller lives in `rise-backend-docker`)
   - **ECR Integration** (`ecr/`): AWS ECR repository management
   - **Encryption** (`encryption/`): Local AES-GCM and AWS KMS providers
   - **OCI Client** (`oci/`): OCI registry interaction
   - **Frontend** (`frontend/`): Serves the built web UI assets
   - **API Layer**: RESTful endpoints via Axum
- **`src/cli/`**: CLI command handlers (feature: `cli`)
   - `login`, `project`, `team`, `deployment`, `env`, `environment`, `domain`, `extension`,
     `service_account`, `identity`, `encrypt`, `run`, `compose`, `skill`, `version`, and
     `backend` (server/controller entrypoints, feature: `backend`)
- **`src/build/`**: Container image build orchestration (feature: `cli`)
   - Docker, Pack (buildpacks), and Railpack backends
   - BuildKit daemon management, SSL certificate handling, registry/proxy plumbing
- **`src/api/`**: Client-side API interface for server communication (feature: `cli`)
- **`src/rise_toml.rs`**: Compatibility shim re-exporting `rise-deployment-spec`; the `rise.toml` model itself lives in `crates/rise-deployment-spec/src/project_config.rs`

Other top-level directories:

- **`frontend/`**: The React web UI (built and served by `src/server/frontend/`)
- **`config/`**: Layered backend settings YAML (`default.yaml`, then run-mode, then `local.yaml`)
- **`migrations/`**: `rise-deploy`'s SQLX migrations (support crates own theirs)
- **`helm/`**: The `rise` Helm chart, including generated CRDs
- **`modules/`**: Terraform modules (e.g. `rise-aws` for the required IAM resources)
- **`skills/`**: Rise skills for AI assistants, installed via `rise skill install`
- **`docs/`**: Two Astro Starlight sites — `docs/user` and `docs/engineering`
- **`tests/`**: e2e harness and fixtures

### Feature Flags

The crate uses Cargo features for modular compilation:

- **`cli`** (default): CLI commands and client-side functionality
- **`backend`**: All server-side functionality including:
  - HTTP server, controllers, and backend logic
  - Kubernetes deployment controller and the Docker deployment backend (bollard/Traefik)
  - Generic resource API and its PostgreSQL store; cross-replica runtime sync; authorization policy
  - Container registry providers and AWS ECR/KMS integration
  - Snowflake OAuth provisioner

Examples:

```bash
cargo build                    # CLI-only build (smallest binary)
cargo build --features backend # Server with all backend capabilities
cargo build --all-features     # Full build with CLI + backend
```

## Implemented Capabilities

1. **Core Infrastructure** ✅
   - [x] Cargo workspace; primary crate feature-gated (`cli`, `backend`)
   - [x] PostgreSQL database with SQLX (compile-time verified queries and migrations)
   - [x] Dex OAuth2/OIDC integration for authentication
   - [x] Docker Compose setup for local development (PostgreSQL, Dex, Registry)

2. **Server Implementation** (`--features backend`) ✅
   - [x] Axum-based HTTP API with RESTful endpoints
   - [x] Authentication: OAuth2/OIDC with Dex, JWT validation, PKCE flow, Rise-issued tokens
   - [x] Project management: CRUD operations, ownership, access classes
   - [x] Team management: Team creation, membership, role-based access
   - [x] Environments, environment variables (plain + encrypted), custom domains
   - [x] Deployment backends:
     - [x] Kubernetes controller — K8s deployments with Ingress
     - [x] Docker backend — single-host Docker daemon with Traefik routing
   - [x] Container registry integration: ECR, GitLab, JFrog, generic OCI basic-auth
   - [x] Encryption providers: Local AES-GCM and AWS KMS
   - [x] OCI client for image digest resolution
   - [x] Frontend web UI
   - [x] Generic resource API (`/api/v1/resources`) with PostgreSQL store and GC
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
     - [x] AWS S3 extension for bucket provisioning
     - [x] Snowflake OAuth provisioner for Snowflake security integrations

3. **CLI Implementation** (`--features cli`, default) ✅
   - [x] OAuth2 authorization code flow with PKCE (browser-based, default) and device flow
   - [x] Project commands: `create`, `list`, `show`, `update`, `delete`
   - [x] Team commands: `create`, `list`, `show`, `update`, `delete`
   - [x] Deployment commands: `create`, `list`, `show`, `stop`, `logs`
   - [x] Environment commands: `create`, `list`, `show`, `update`, `delete`
   - [x] Environment variable management (`set`, `get`, `list`, `delete`, `import`, `export`)
   - [x] Custom domain management (`add`, `list`, `remove`)
   - [x] Extension management (`create`, `update`, `patch`, `list`, `show`, `delete`)
   - [x] Service account (workload identity) management and in-workload `identity token`
   - [x] Skill installation for AI assistants (`skill install`)

4. **Build System** (`--features cli`) ✅
   - [x] Docker backend: Standard Dockerfile builds
   - [x] Pack backend: Cloud Native Buildpacks integration
   - [x] Railpack backend: Railpacks with BuildKit/Buildx
   - [x] Automatic build method detection
   - [x] BuildKit daemon management with SSL certificate handling
   - [x] `rise build` command for local image builds without deployment
   - [x] `rise run` command for local development (build and run with docker/podman)
   - [x] `rise compose up` for running multi-container projects locally
   - [x] Pre-built image deployment support (`--image` flag)
   - [x] Deployment following with auto-refresh and timeout support

## Documentation

Documentation lives in two Astro Starlight sites under [`/docs`](./docs):

- **User docs** — [`docs/user/src/content/docs/`](docs/user/src/content/docs/)
  - Build backends (Docker, Pack, Railpack): [user-guide/builds.md](docs/user/src/content/docs/user-guide/builds.md)
  - SSL & proxy configuration: [user-guide/ssl-proxy.md](docs/user/src/content/docs/user-guide/ssl-proxy.md)
  - Project configuration: [user-guide/configuration.mdx](docs/user/src/content/docs/user-guide/configuration.mdx)
  - OAuth extension (end-user authentication): [user-guide/oauth.md](docs/user/src/content/docs/user-guide/oauth.md)
- **Engineering / operator docs** — [`docs/engineering/src/content/docs/`](docs/engineering/src/content/docs/)
  - Architecture and process design: [development.md](docs/engineering/src/content/docs/development.md)
  - Backend configuration: [configuration.md](docs/engineering/src/content/docs/configuration.md)
  - Deployment backends and the feature matrix: [deployment-backends.md](docs/engineering/src/content/docs/deployment-backends.md)
  - Generic resource API: [generic-resource-api.md](docs/engineering/src/content/docs/generic-resource-api.md)
  - Architecture decision records: [adr/](docs/engineering/src/content/docs/adr/)

Serve them locally with `mise run docs:serve` / `mise run docs:engineering:serve`.

## Git Branching

- The default development branch is `develop`. PRs for feature work should target `develop`, not `main`.
- Always target the branch your feature branch was created from when opening a PR.

## Rollout Tracking

High-impact, multi-PR, or operator-affecting changes are tracked in the **Rise
Rollout Tracker** GitHub Project: <https://github.com/orgs/rise-deploy/projects/1>.
Consult it when planning or reviewing large or breaking work to see in-flight
workstreams, their phase, and outstanding finalization gates (deferred steps that
flip a default, drop a compat shim, or tighten a constraint).

Keep it current as work merges:

- When a PR advances a tracked workstream, add it to the Project, link it from the
  PR body, and move the item's `Status`. When you defer a finalization step, file
  it as a `rollout-gate` issue and add it to the Project.
- Set `Workstream`, `Breaking?`, `Operator impact`, and `Target release` on every
  item. If `Operator impact` is not `None`, the item is **not** `Done` until the
  operator [Upgrade Notes](docs/engineering/src/content/docs/upgrade-notes.md) page
  has a matching entry for that release.
- `ROADMAP.md` owns the *why*, phase rationale, and milestone checkboxes for every
  in-flight architectural workstream (its numbered sections: generic resource and
  authorization foundation, Rise-issued authentication and token issuance,
  subresource execution, typed-object migration, external controllers and
  multi-org routing, codebase decomposition). The Project owns *live status*.
  Don't duplicate rationale into the board, and don't create new
  `<TOPIC>_PLAN.md` / `<TOPIC>_ROADMAP.md` files.
  Architectural decisions are recorded as ADRs under
  `docs/engineering/src/content/docs/adr/` — an ADR records the decision, its
  rationale, and (via its Status field) implementation progress. Prefer an
  ADR over a new `ROADMAP.md` section for new architectural work; `ROADMAP.md`
  is being wound down gradually and keeps covering only the workstreams
  already tracked in it.

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
- Admin users (`auth.admin_users`) bypass the regular permission checks on the typed APIs (projects, teams, deployments, etc.) — they have full access there without passing ownership/membership checks. This does **not** extend to the generic resource API (`/api/v1/resources`), which is operator-gated (`auth.operator_users`): admins are not operators and do not bypass its checks. Granting admins access to the resource API is intentionally deferred (see `ROADMAP.md`).
- In `rise-deploy`, all SQLX queries must be wrapped by helper functions in the `src/db/` module — no SQLX queries elsewhere in the crate's production code. The support crates that own their own schema and migrations (`rise-resource-store-postgres`, `rise-runtime-sync`) are the exception: their queries live in the crate that owns the tables, alongside its own migrations and offline query cache.
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

**MANDATORY**: You MUST run `cargo fmt --all` before every commit. Always. No exceptions. CI will reject unformatted code. Run it, stage any formatting changes, then commit. If you touched `tests/e2e`, that crate is a **separate workspace** — `--all` does not reach it, so also run `cargo fmt --manifest-path tests/e2e/Cargo.toml`.

**Always run** (fast, catches most issues):

```bash
cargo fmt --all                # Format code — MUST run before every commit
cargo clippy --all-features --all-targets -- -D warnings  # Lint (uses cached build artifacts)
```

**Run selectively** based on what changed:

| What changed | Command | Why |
|---|---|---|
| Any `.rs` file | `cargo test --workspace --all-features` | Unit tests (requires `mise run db:migrate` once); `--workspace` covers the support crates but **not** `tests/e2e` |
| `tests/e2e/**` | `cargo fmt --manifest-path tests/e2e/Cargo.toml`, then `cargo clippy --manifest-path tests/e2e/Cargo.toml --all-targets -- -D warnings` and `cargo test --manifest-path tests/e2e/Cargo.toml` | Standalone workspace, checked by CI in its own job |
| SQLX queries in `rise-deploy` | `mise run sqlx:prepare` | Regenerate offline query cache (commit the result) |
| SQLX queries in `rise-resource-store-postgres` | `mise run resource-store-postgres:sqlx:prepare` | Crate-local offline cache (commit the result) |
| SQLX queries in `rise-runtime-sync` | `mise run runtime-sync:db:migrate` once, then `mise run runtime-sync:sqlx:prepare` | Crate-local offline cache; needs the `runtime_sync` schema applied |
| Dependencies (`Cargo.toml`/`Cargo.lock`) | `cargo audit` | CI fails on advisories |
| Server settings structs (`src/server/settings.rs`) | `mise run config:schema:generate` | Regenerate `docs/engineering/public/schemas/backend-settings.schema.json` (commit the result) |
| `rise.toml` structs (`crates/rise-deployment-spec/src/project_config.rs`) | `mise run rise-toml:schema:generate` | Regenerate `docs/user/public/schemas/rise-toml-v1.schema.json` (commit the result) |
| Resource API types (`crates/rise-resource-api/`) | `mise run resource:schema:generate` | Regenerate the resource/organization/controller-status schemas under `docs/engineering/public/schemas/` (commit the result) |
| CRD structs (`src/server/deployment/crd.rs`) | `mise run crd:generate` | Regenerate `helm/rise/crds/riseproject-crd.yaml` (commit the result) |
| Helm chart (`helm/rise/`) | `helm lint helm/rise` | Validate chart templates |

**Full CI-equivalent check** (slower, runs everything):

```bash
mise run lint                       # See below for what this covers
mise run config:schema:check        # Verify backend config schema is up to date
mise run rise-toml:schema:check     # Verify rise.toml schema is up to date
mise run crd:check                  # Verify CRD YAML matches Rust definition
cargo audit                         # Dependency advisories
cargo test --workspace --all-features  # Unit tests (all workspace crates)
```

`mise run lint` runs, in order: `cargo all-features check --all-targets`; a
per-crate `cargo check` for `rise-authz`, `rise-resource-api`,
`rise-resource-store-postgres`, `rise-backend-auth`, `rise-backend-core`, and
`rise-runtime-sync`; `cargo all-features clippy -- -D warnings` plus the same
per-crate clippy sweep; `cargo fmt --all -- --check`; `mise sqlx:check`;
`mise resource:schema:check`; `helm lint helm/rise`; and `cargo test` for
`rise-authz`, `rise-backend-auth`, and `rise-backend-core`. It does **not** cover
`config:schema:check`, `rise-toml:schema:check`, `crd:check`, `cargo audit`, or
`tests/e2e` — CI does, so run those separately.

## Deployment Backend Parity (STRICT)

Rise supports multiple deployment backends (currently **Kubernetes** and
**Docker**). They are equal first-class citizens, and we aim for **semantic
feature parity and correctness across all of them**.

**The rule:** any feature configurable through a public Rise API surface
(`rise.toml`, project/deployment settings, environment variables, the HTTP API)
must, *where technically possible*, be supported by **every** backend, and must
behave the **same way** on each. A gap that is a fundamental limitation of a
backend (e.g. a single-host Docker daemon has no horizontal scale-out or
per-workload network policy) is acceptable and must be documented; a gap that is
merely unimplemented is a **parity bug** to track and close.

**How to apply it:**

- **Do not implicitly plan the cross-backend work.** When a feature is requested
  or implemented for one backend, do **not** silently extend it to the others.
  Implement what was asked.
- **Do raise the flag — explicitly.** During **planning** and **review**, call
  out the parity implication: "this is configurable via <public API> and is now
  supported on <backend A> but not <backend B> — is that an intentional
  limitation or a parity gap to track?" Surface it; let the human decide.
- **Keep the feature matrix current.** The source of truth is the
  [Deployment Backends](docs/engineering/src/content/docs/deployment-backends.md)
  overview page (`/operator-docs/deployment-backends/`). Any change that adds or
  alters a backend feature must update the matrix in the **same** change — add a
  row for a new feature, flip a cell when support changes, and note the reason
  for any `⚠️`/`❌`.

## Ingress Authentication

Ingress-level authentication is driven by each project's **access class** (`access_requirement`: `None` / `Authenticated` / `Member`). The Kubernetes controller is fully wired: for non-`None` access requirements it stamps nginx auth annotations (`nginx.ingress.kubernetes.io/auth-url` → `/api/v1/auth/ingress`, `auth-signin`, `auth-response-headers`) — see `ResourceBuilder::build_ingress_annotations` in `src/server/deployment/resource_builder.rs`. The subrequest is served by the `ingress_auth` handler in `src/server/auth/handlers.rs`, which validates the Rise JWT session cookie, then enforces `Authenticated` (any logged-in user) or `Member` (project owner/team member) and returns `X-Auth-Request-Email`/`X-Auth-Request-User` on success. The Docker backend enforces the same access classes via Traefik forwardAuth against the same handler.
