# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Application developers inside an organisation that runs its own Rise install as an internal platform (PaaS). They deploy and operate their own apps, mostly from the `rise` CLI and CI pipelines, and open the web dashboard to see what is running, diagnose a failed deploy, roll back or promote, and manage variables, domains, environments, teams and access.

The platform/operator team that runs the install is a secondary audience (admins, operators); the dashboard is not designed around them.

## Product Purpose

Rise deploys apps packaged as container images to a container runtime from the most minimal configuration. The CLI builds the image locally, pushes it with temporary registry credentials issued by the backend, and asks the backend to deploy it. The dashboard makes the resulting state legible and lets developers act on it without the CLI.

Success: a developer can tell at a glance what each environment of their project is serving and whether it is healthy, and can recover from a bad deploy (diagnose, roll back, redeploy) in a few clicks.

## Positioning

- **Minimal config, CLI-first.** A project deploys from a near-empty `rise.toml`; builds run locally (Dockerfile, Cloud Native Buildpacks, Railpack). The dashboard complements the CLI rather than replacing it.
- **Runtime-agnostic.** Kubernetes, Docker and Amazon ECS are equal first-class deployment backends with semantic feature parity. The UI never assumes one runtime.
- **Enterprise-ready.** SSO via OAuth2/OIDC, workload identity for CI (service accounts), access classes enforced in front of deployed apps, corporate MITM proxy support, operator-run installs.
- **Self-hosted internal platform.** Each organisation runs its own install on its own domain; apps are reached under the install's common domain (e.g. `https://<project>.<domain>`). There is no shared Rise cloud.
- **Multi-tenant later.** The install is meant to become multi-tenant: organisations are being introduced through the generic resource API (ADR-0001/0004, `ROADMAP.md` §5, multi-org routing). Today's UI has one implicit organisation; future surfaces must leave room for an organisation scope.

## Operating Context

- Developers deploy with `rise deploy` from their machine or CI (`-E <environment>`, `--group <group>`, `--expire`), then check the result in the dashboard.
- A **project** is the unit of deployment and has **environments** (production is created with every project). Each environment serves the active deployment of its **primary deployment group**; other groups (merge-request previews, branches) run beside it on their own URLs and can expire.
- Deployments move through Pending → Building → Pushing → Pushed → Deploying → Healthy (or Unhealthy / Failed / Cancelled / Stopped / Superseded / Expired). The CLI reports build and push; the controller reports the rest.
- Redeploy, rollback and promote create a new deployment from an existing image without rebuilding.
- Logs are per deployment.

## Capabilities and Constraints

- **Names, not IDs.** Projects, teams, environments and deployments are addressed by name everywhere (URLs, CLI); internal UUIDs never surface.
- **Sign-in is SSO only** (OIDC). There is no password login and no user invitation flow: a person can be added to a team only after they have signed in to Rise once.
- **Teams** have owners and members (a person can be both); IdP-managed teams are read-only for non-admins.
- **Variables** are project-wide (Global) or per environment, where an environment value overrides Global. Secrets are encrypted and masked; protected secrets are write-only and can never be read back. Variables are snapshotted per deployment and apply on the next deploy. There is no audit trail of who changed a variable.
- **Access classes** (configured per install) decide who can reach a deployed app.
- **Extensions** attach managed resources and integrations (OAuth providers, AWS RDS, AWS S3, Snowflake).
- No commit message or SHA is stored on deployments; image, digest, creator, CI job and pull/merge request links are.
- Admin users bypass permission checks on the typed APIs but not on the generic resource API.

## Brand Commitments

- Name: **Rise**. Logo and favicon in `static/assets/` (`logo.svg`, `favicon-32x32.png`).
- Voice: clear, concise feedback at each step; the product's own terms (project, environment, deployment group, deployment, access class, extension, team, owner, service account) used consistently in CLI and dashboard.

## Evidence on Hand

- Dashboard screenshots in `docs/screenshots/`.
- User and operator documentation in `docs/user` and `docs/engineering`.
- No customer logos, testimonials, usage numbers or pricing exist; future work must not invent them.

## Product Principles

1. **State first.** What is serving, whether it is healthy and what changed must be readable before any action is offered.
2. **Truth over the mock.** Show only what the backend actually knows and does; never imply data (authors of changes, commit messages, invitations) or behaviour Rise does not have.
3. **Recovery in reach.** Failed deploys come with their reason, what is still serving, and the one-click way back.
4. **The dashboard and the CLI are one product.** Same names, same concepts; the UI points to the CLI command when the CLI is where the work happens.
5. **Every runtime, same experience.** Features behave identically on Kubernetes, Docker and ECS; gaps are documented, not designed around silently.

## Accessibility & Inclusion

Best effort, no formal target: keep the UI accessible as a matter of craft (contrast, keyboard access, visible focus, labelled controls) without committing to a standard.
