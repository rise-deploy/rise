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
| `rise vault` | | `configure`, `show`, `logout` | [Client-Controlled Push](../client-push#vault-minted-jfrog-token-laptop) |
| `rise encrypt` | | | [OAuth Extensions](../oauth) |
| `rise backend` | | `server`, `check-config`, `config-schema` | Operator commands (requires build with `--features backend`) |

`rise deploy` is a shortcut for `rise deployment create`.

## Project Name Resolution

Most commands accept `-p <project>` to specify the project name. If omitted, Rise reads the project name from `rise.toml` or `.rise.toml` in the current directory (or the path specified by `--path`).

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RISE_URL` | Default backend URL |
| `RISE_TOKEN` | Authentication token (skips interactive login) |
| `RISE_CONTAINER_CLI` | Container CLI: `docker` or `podman` |
| `RISE_MANAGED_BUILDKIT` | Enable managed BuildKit daemon (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_NETWORK_NAME` | Docker network for managed BuildKit daemon |
| `RISE_MANAGED_BUILDKIT_HOST_NETWORK` | Run managed BuildKit with host networking (`true`/`false`) |
| `RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES` | Comma-separated list of insecure registries |
| `SSL_CERT_FILE` | CA certificate file for SSL builds |
| `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` | Proxy settings (auto-injected into builds) |

## Global Configuration

CLI settings are stored in `~/.config/rise/config.json`, created on first `rise login`. See [Project Configuration](../configuration#global-cli-config) for details.
