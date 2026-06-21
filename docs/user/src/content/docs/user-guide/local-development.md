---
title: "Local Development"
---

The `rise run` command builds and runs your application locally in a container, simulating a deployment environment for development and testing.

## Basic Usage

```bash
# Build and run from current directory (port 8080)
rise run

# Specify directory
rise run ./path/to/app
```

## Port Configuration

- `--http-port` — the port your application listens on inside the container (also sets the `PORT` env var)
- `--expose` — the port exposed on your host machine (defaults to same as `--http-port`)

```bash
# Application listens on 3000, accessible at http://localhost:3000
rise run --http-port 3000

# Application listens on 8080, accessible at http://localhost:3000
rise run --http-port 8080 --expose 3000
```

## Project Environment Variables

Load environment variables from a Rise project:

```bash
rise run --project my-app
```

This is enabled by default when `--project` is specified. The CLI fetches the full set of environment variables your deployment would receive, including:

- **User-set variables** — plain and secret (decrypted) project env vars
- **System variables** — `PORT`, `RISE_ISSUER`, `RISE_APP_URL`, `RISE_APP_URLS`
- **Extension variables** — OAuth `CLIENT_ID`/`CLIENT_SECRET`/`ISSUER`, etc.

Protected secrets (e.g., RDS database credentials) cannot be loaded locally and are skipped with a warning.

Disable with `--use-project-env=false`.

For OAuth extension support during local development, see [OAuth — Local Development](../oauth#local-development).

## Runtime Environment Overrides

Set or override environment variables for the local run:

```bash
rise run -e DATABASE_URL=postgres://localhost/mydb -e DEBUG=true
```

`--env` / `-e` values take precedence over project environment variables.

## Build Backend Selection

Use any build backend:

```bash
rise run --backend pack
rise run --backend railpack
rise run --backend docker --dockerfile Dockerfile.dev
```

All standard [build flags](../builds) are supported.

## Compose Stacks

`rise compose` runs a project through Docker Compose instead of `docker run`.
Use it when you want the local runtime shape to match a Rise deployment more
closely, or when a project has multiple containers that need to run together.

For a single-container project, `rise compose` builds the top-level app as the
implicit `app` container. `--http-port` is the port your app listens on inside
the container and also sets `PORT`; `--router-port` is the host port published by
the local Traefik router.

```bash
# Build and run a single-container project through Compose
rise compose up

# App listens on 3000, router published at http://localhost:8080
rise compose up --http-port 3000

# App listens on 3000, router published at http://localhost:3000
rise compose up --http-port 3000 --router-port 3000
```

For a project that declares a [`[containers]`](../deployments#multi-container-deployments)
table, `rise run` still runs only one selected container, while `rise compose up`
runs the whole stack. Container ports come from `[containers.<name>].port`; the
`--http-port` flag is only used for single-container projects.

### Run One Container

Pick which container to build and run with `--container`:

```bash
rise run --container api
```

The container's own `port` sets `PORT` and the host mapping, and its
`[containers.api.env]` overrides are layered on top of the project env vars.
Running `rise run` without `--container` on a multi-container project errors and
lists the available container names.

### Run The Stack

`rise compose` builds local images and runs them together via Docker Compose,
mirroring production: siblings reach each other by service name and receive the
same `RISE_CONTAINER_HOST__<NAME>` variables, and path-based `[routes]` are
replicated by a [Traefik](https://traefik.io/) router published on a single host
port.

```bash
# Build and run the stack (Ctrl+C tears it down)
rise compose up

# Publish the router on a different host port
rise compose up --router-port 3000

# Run in the background, then stop later
rise compose up --detach
rise compose down
```

Inspect a running stack without dropping to the Docker CLI:

```bash
rise compose ps                       # list the stack's containers
rise compose logs                     # show logs from all containers
rise compose logs -f                  # follow
rise compose logs -c api --tail 100   # just the api container, last 100 lines
```

Routing is label-driven (no config file is mounted); the router needs access to
the Docker socket (`/var/run/docker.sock`). Because the Traefik router mounts
this socket, `rise compose` assumes a Docker-compatible runtime. Podman users
may need additional socket configuration, such as enabling the podman socket and
pointing it at `/var/run/docker.sock`. Containers with a `port` but no route
(e.g. a database) are reachable by siblings on the internal network but are not
published to the host.

To customize the Compose file, write it to disk instead of running it:

```bash
rise compose generate                 # writes ./compose.yaml
rise compose generate --stdout        # print to stdout
rise compose generate -o my-compose.yaml
```

Then run it yourself with `docker compose -f compose.yaml up` after the images
are built (`rise compose up` builds them for you).

## Standalone Image Build

Build an image without running it:

```bash
rise build myapp:latest
rise build myapp:latest --backend pack
```

Push the built image to a registry:

```bash
rise build myapp:latest --push
```

## Running Without a Container

If your workflow runs the app directly (e.g. `cargo run`, `npm run dev`) rather than in a container, use `rise env export` to inject Rise's environment variables into your shell:

```bash
rise env export -p my-app > .env.rise
# Load with your preferred tool:
export $(cat .env.rise | xargs)
# or: source .env.rise, direnv, dotenv, etc.
```

`rise env export` outputs the resolved set of non-secret environment variables — `PORT`, `RISE_ISSUER`, `RISE_APP_URL`, `RISE_APP_URLS`, and any user-set project variables. No image build is required.

:::note
Variables from extensions (e.g., RDS database credentials) are protected secrets and are not included. Use a local database instead and override `DATABASE_URL` in your shell.
:::

## How It Works

1. Builds the container image using the selected backend
2. Tags the image as `rise-local-{project-name}`
3. Fetches the full deployment preview env vars from the project (if specified) — including user vars, system vars, and extension-injected vars
4. Runs `docker run --rm -it -p {expose}:{http-port}` with the image
5. Sets `PORT` environment variable (CLI `--http-port` flag takes precedence)
6. Container is removed when stopped (`--rm`)

Press Ctrl+C to stop the container.
