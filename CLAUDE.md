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

**Note**: The project is a Cargo **workspace**. The primary crate `rise-deploy` produces the `rise` binary with both CLI and server capabilities enabled via feature flags. A few focused, backend-only support crates live under `crates/` and are depended on as optional, `backend`-feature-gated path deps: `rise-resource-api` / `rise-resource-store-postgres` (generic resource API contract / PostgreSQL adapter), `rise-backend-auth` (pure-core token signing, verification, and matching — the single home for auth-token logic; see `ROADMAP.md` § "Authentication & Token Exchange"), `rise-backend-core` (the deployment-backend contract seam: shared deployment models, the `DeploymentBackend` trait, registry/encryption provider traits, the pure `quantity`/`state_machine`/`runtime`/`url_builder`/`token_ttl`/`custom_domain` helpers, and the `DeploymentStore` trait — the database boundary implemented by `rise-deploy`'s `PgDeploymentStore`), and `rise-backend-docker` (the Docker deployment backend: `DockerBackend` + the in-process `DockerReconciler`, the first controller extracted onto the `rise-backend-core` seam — see issue #377; the Kubernetes controller is still in-tree pending the same treatment). `rise-backend-docker` is re-exported under `crate::server::deployment::controller::docker`, so existing module paths keep resolving.

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

**Project Structure**: A Cargo workspace whose primary crate `rise-deploy` carries both CLI and backend (feature-gated). Focused, backend-only support crates live under `crates/` — `rise-resource-api` / `rise-resource-store-postgres` and the pure-core `rise-backend-auth` (the single home for auth-token signing, verification, and matching).

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

## Database Migrations

A migration becomes immutable when it ships in a **release**, not when it merges
to `develop`.

- **Unreleased** (merged to `develop` or not): fully editable. Rewrite the file,
  renumber it, split it, delete it, or **collapse a whole series into one** —
  whatever leaves the clearest final schema. Don't stack a corrective migration
  on top of an unreleased one, and don't preserve an increment just because it
  was reviewed separately; the migration history is a means, not a record.
- **Released**: the file is frozen. Change it only by adding a new migration.

**Release candidates count as releases here** — an `-rc` tag can be deployed, so
a migration that ships in one is frozen.

To check which side a migration is on, look for it in every tag's tree:

```bash
BASE=20260519000000_create_resource_store.sql
for t in $(git tag --list 'v*'); do
  git ls-tree -r --name-only "$t" | grep -q "/$BASE\$" && echo "$t"
done
# no output = in no release = editable
```

Match on the basename, not the full path, so a crate rename doesn't hide a
released migration. Don't reach for `git tag --contains <adding-commit>`: it
reports "unreleased" for migrations that really did ship, because release tags
do not necessarily descend from the `develop` commit that added the file. And
don't pick a "latest tag" with `sort -V` — it orders `v0.23.0-rc4` *after*
`v0.23.0`.

SQLX records a checksum per migration, so editing one a database has already
applied fails startup with `VersionMismatch` ("previously applied but has been
modified"), and removing one fails with `VersionMissing`. That is exactly why
the released/unreleased line matters — and why it is drawn at *release*, not at
*merge*: before a release, the only databases holding the old shape are
development and CI ones that can be rebuilt.

Rebuilding after an edit or collapse: drop what the migrations created and let
them re-run. For the resource store that is `DROP SCHEMA resource_store
CASCADE;` — its migration bookkeeping lives in that schema, so dropping it
clears both. For the main crate, delete the affected rows from `_sqlx_migrations`
and drop whatever the migration created.

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
- `ROADMAP.md` owns the *why*, phase rationale, and milestone checkboxes
  for every in-flight architectural workstream (multi-tenancy + generic
  resource API, authentication & token exchange, future workstreams). The
  Project owns *live status*. Don't duplicate rationale into the board, and
  don't create new `<TOPIC>_PLAN.md` / `<TOPIC>_ROADMAP.md` files.
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
| Any `.rs` file | `cargo test --workspace --all-features` | Unit tests (requires `mise run db:migrate` once); `--workspace` ensures crates such as `rise-backend-auth` and `rise-backend-core` are included |
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

Ingress-level authentication is driven by each project's **access class** (`access_requirement`: `None` / `Authenticated` / `Member`), not the legacy `visibility` field. The Kubernetes controller is fully wired: for non-`None` access requirements it stamps nginx auth annotations (`nginx.ingress.kubernetes.io/auth-url` → `/api/v1/auth/ingress`, `auth-signin`, `auth-response-headers`) — see `ResourceBuilder::build_ingress_annotations` in `src/server/deployment/resource_builder.rs`. The subrequest is served by the `ingress_auth` handler in `src/server/auth/handlers.rs`, which validates the Rise JWT session cookie, then enforces `Authenticated` (any logged-in user) or `Member` (project owner/team member) and returns `X-Auth-Request-Email`/`X-Auth-Request-User` on success.
