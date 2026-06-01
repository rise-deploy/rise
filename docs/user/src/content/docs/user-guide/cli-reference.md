---
title: "CLI Reference"
---

The Rise CLI (`rise`) provides commands for managing projects, deployments, teams, and more. Use `rise --help` or `rise <command> --help` for full flag details.

## Commands

| Command | Alias | Subcommands | Details |
|---------|-------|-------------|---------|
| `rise login` | | | [Authentication](../authentication) |
| `rise deploy` | | | [Deployments](../deployments) |
| `rise build` | | | [Building Images](../builds) |
| `rise run` | | | [Local Development](../local-development) |
| `rise project` | `p` | `create` (`c`), `list` (`ls`), `show` (`s`), `update` (`u`), `delete` (`rm`) | [Configuration](../configuration) |
| `rise project app-user` | | `add` (`a`), `list` (`ls`), `remove` (`rm`) | [Authentication](../authentication#app-users) |
| `rise deployment` | `d` | `create` (`c`), `list` (`ls`), `show` (`s`), `stop`, `logs` | [Deployments](../deployments) |
| `rise environment` | `envs` | `create` (`c`), `list` (`ls`), `show` (`s`), `update` (`u`), `delete` (`rm`) | [Environments](../environments) |
| `rise env` | `e` | `set` (`s`), `list` (`ls`), `get` (`g`), `delete` (`rm`), `import` (`i`), `show-deployment` | [Environment Variables](../environment-variables) |
| `rise domain` | `dom` | `add` (`a`), `list` (`ls`), `remove` (`rm`) | [Custom Domains](../custom-domains) |
| `rise team` | `t` | `create` (`c`), `list` (`ls`), `show` (`s`), `update` (`u`), `delete` (`rm`) | |
| `rise service-account` | `sa` | `create` (`c`), `list` (`ls`), `show` (`s`), `delete` (`rm`) | [Service Accounts](../service-accounts) |
| `rise extension` | `ext` | `create` (`c`), `update` (`u`), `patch` (`p`), `list` (`ls`), `show` (`s`), `delete` (`rm`) | [OAuth Extensions](../oauth) |
| `rise encrypt` | | | [OAuth Extensions](../oauth) |
| `rise backend` | | `server`, `check-config`, `config-schema` | Operator commands (requires build with `--features backend`) |

`rise deploy` is a shortcut for `rise deployment create`.

## Project Name Resolution

Commands that operate on a project accept the project name as a positional argument (e.g. `rise project show my-app`), and deployment and environment commands also accept it via `-p <project>`. If omitted, Rise reads the project name from the `[project]` section of `rise.toml` or `.rise.toml` in the directory given by `--path` (defaults to the current directory).

This means `rise project show`, `update`, and `delete` can be run with no project name when a `rise.toml` is present:

```bash
# Equivalent when rise.toml has [project] name = "my-app"
rise project show my-app
rise project show
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RISE_URL` | Default backend URL |
| `RISE_TOKEN` | Authentication token (skips interactive login) |
| `RISE_TOKEN_COMMAND` | Shell command whose stdout is used as the bearer token. JWT output uses the embedded `exp` claim; opaque output uses `RISE_TOKEN_COMMAND_TTL` as its assumed lifetime. |
| `RISE_TOKEN_COMMAND_TTL` | Assumed lifetime (seconds) for opaque tokens produced by `RISE_TOKEN_COMMAND`. The command is re-run after two thirds of this TTL has elapsed. JWT tokens ignore this setting and use their `exp` claim instead. Default: `600` (10 minutes). |
| `RISE_TOKEN_COMMAND_TIMEOUT` | Maximum runtime (seconds) for `RISE_TOKEN_COMMAND` before the CLI kills it and treats the attempt as failed. Default: `10`. |
| `RISE_GHA_AUDIENCE` | Audience for GitHub Actions OIDC token minting (auto-detected from `ACTIONS_ID_TOKEN_REQUEST_URL`). Recommended value: the Rise server URL. |
| `RISE_CONTAINER_CLI` | Container CLI: `docker` or `podman` |
| `RISE_MANAGED_BUILDKIT` | Enable managed BuildKit daemon (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_NETWORK_NAME` | Docker network for managed BuildKit daemon |
| `RISE_MANAGED_BUILDKIT_HOST_NETWORK` | Run managed BuildKit with host networking (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES` | Comma-separated list of insecure registries |
| `RISE_REGISTRY_CRED_MIN_LIFETIME_SECS` | Minimum remaining lifetime (seconds) that registry credentials must have to be reused across containers in a multi-container build; credentials expiring sooner are re-minted. Default: `1200` (20 minutes). |
| `SSL_CERT_FILE` | CA certificate file for SSL builds |
| `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` | Proxy settings (auto-injected into builds) |

### Token source precedence

The CLI picks its backend bearer token from the first available of these sources, in this **fixed** order (not currently user-configurable):

1. `RISE_TOKEN` — explicit token, always wins.
2. `RISE_TOKEN_COMMAND` — stdout of the given shell command.
3. GitHub Actions OIDC — auto-detected from `ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN`; requires `RISE_GHA_AUDIENCE`.
4. Stored login token from `rise login` (in the config file).

Caveat: because GitHub Actions OIDC (3) is auto-detected and ranks above the stored login token (4), a workflow with `id-token: write` and no `RISE_TOKEN`/`RISE_TOKEN_COMMAND` will use OIDC and **error if `RISE_GHA_AUDIENCE` is unset** — it does not fall back to a stored token. Setting `RISE_TOKEN` overrides everything.

### Token refresh policy

For token sources that can mint new tokens, the CLI refreshes proactively so long deploys do not run into token expiry mid-build or while following deployment status:

- JWT tokens: the CLI reads the `exp` claim and refreshes after two thirds of the observed lifetime has elapsed (`exp - time minted`). It also refreshes once the token is within 60 seconds of `exp`, whichever happens first.
- GitHub Actions OIDC: the minted token is a JWT, so it follows the JWT policy above. For a typical ~5 minute GitHub Actions ID token, the CLI refreshes after roughly 3 minutes 20 seconds.
- `RISE_TOKEN_COMMAND` returning a JWT: the JWT `exp` policy wins; `RISE_TOKEN_COMMAND_TTL` is ignored.
- `RISE_TOKEN_COMMAND` returning an opaque token: the CLI treats `RISE_TOKEN_COMMAND_TTL` as the token lifetime and re-runs the command after two thirds of that TTL has elapsed.
- `RISE_TOKEN_COMMAND` must complete within `RISE_TOKEN_COMMAND_TIMEOUT` seconds on each attempt.
- `RISE_TOKEN` and stored login tokens: these are treated as static values by the CLI and are not re-minted.

## Global Configuration

CLI settings are stored in `~/.config/rise/config.json`, created on first `rise login`. See [Project Configuration](../configuration#global-cli-config) for details.
