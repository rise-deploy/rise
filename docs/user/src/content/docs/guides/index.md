---
title: Guides
description: Task-focused guides for building, deploying, and connecting applications on Rise.
---

These guides explain application workflows, from choosing a build backend to automating deployments and configuring end-user access.

If you are new to Rise, start with [your first deployment](/docs/user-guide/getting-started/).

## Common workflows

- [Deploying from CI](/docs/guides/deploying-from-ci/) — automate deployments using a service account.
- [Authenticating end users](/docs/guides/authenticating-end-users/) — add sign-in and validate application identity tokens.
- [Choosing a build backend](/docs/guides/choosing-a-build-backend/) — compare Docker, Pack, and Railpack for your application.
- [Using Rise with AI agents](/docs/guides/ai-app-builder/) — give a coding agent the context and tooling it needs to deploy your application.

## Builds and deployments

Read [building images](/docs/user-guide/builds/) for build configuration and [deployments](/docs/user-guide/deployments/) for the deployment lifecycle. [Environments](/docs/user-guide/environments/) describes production and preview deployments; [CI/CD setup](/docs/user-guide/ci-cd/) covers pipeline configuration.

Use [environment variables](/docs/user-guide/environment-variables/) to configure application settings and [custom domains](/docs/user-guide/custom-domains/) to expose an application on your own hostname.

## Identity and access

[Service accounts](/docs/user-guide/service-accounts/) provide credentials for automation. [Workload identity tokens](/docs/user-guide/workload-identity-tokens/) let applications authenticate to services without a long-lived shared credential. [Application authentication](/docs/user-guide/authentication-for-apps/) explains how Rise protects application ingress and passes identity to your application.
