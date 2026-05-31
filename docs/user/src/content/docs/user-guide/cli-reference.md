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
| `RISE_TOKEN_COMMAND` | Shell command whose stdout is used as the bearer token. JWT output uses the embedded `exp` claim; opaque output is cached for `RISE_TOKEN_COMMAND_TTL` seconds. |
| `RISE_TOKEN_COMMAND_TTL` | How long (seconds) to cache an opaque token produced by `RISE_TOKEN_COMMAND` before re-running the command. JWT tokens ignore this setting and use their `exp` claim instead. Default: `600` (10 minutes). |
| `RISE_GHA_AUDIENCE` | Audience for GitHub Actions OIDC token minting (auto-detected from `ACTIONS_ID_TOKEN_REQUEST_URL`). Recommended value: the Rise server URL. |
| `RISE_CONTAINER_CLI` | Container CLI: `docker` or `podman` |
| `RISE_MANAGED_BUILDKIT` | Enable managed BuildKit daemon (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_NETWORK_NAME` | Docker network for managed BuildKit daemon |
| `RISE_MANAGED_BUILDKIT_HOST_NETWORK` | Run managed BuildKit with host networking (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES` | Comma-separated list of insecure registries |
| `RISE_REGISTRY_CRED_MIN_LIFETIME_SECS` | Minimum remaining lifetime (seconds) that registry credentials must have to be reused across containers in a multi-container build; credentials expiring sooner are re-minted. Default: `1200` (20 minutes). |
| `SSL_CERT_FILE` | CA certificate file for SSL builds |
| `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` | Proxy settings (auto-injected into builds) |

## Global Configuration

CLI settings are stored in `~/.config/rise/config.json`, created on first `rise login`. See [Project Configuration](../configuration#global-cli-config) for details.
