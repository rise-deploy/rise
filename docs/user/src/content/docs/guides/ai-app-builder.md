---
title: "Using Rise with AI Agents"
description: "Install or remotely load the Rise App Builder skill so AI agents follow Rise-specific deployment patterns."
---

## When to use this

Use the Rise App Builder skill when an AI agent is building, configuring, or deploying an application on Rise. It provides decision routing, safety invariants, CLI commands, and `rise.toml` guidance that an agent should prefer over general container-platform assumptions.

## Install the skill

Install the bundled skill into the default Claude Code and Crush-compatible skills directory:

```bash
rise skill install
```

Restart the AI tool after installation. Use `rise skill list` to verify the installation or `rise skill install --target <directory>` for another skills location.

## Load the skill remotely

An agent does not need the skill installed locally. Point it to the hosted files:

- [Rise App Builder `SKILL.md`](https://rise-deploy.github.io/rise/user/skills/rise-app-builder/SKILL.md)
- [CLI cheatsheet](https://rise-deploy.github.io/rise/user/skills/rise-app-builder/reference/cli-cheatsheet.md)
- [`rise.toml` reference](https://rise-deploy.github.io/rise/user/skills/rise-app-builder/reference/rise-toml-reference.md)

The user documentation [`llms.txt`](https://rise-deploy.github.io/rise/user/llms.txt) also lists these files under **AI Skills**, so an agent starting from the documentation root can discover them automatically.

:::tip[Recommended prompt]
Read the Rise user documentation `llms.txt` and the linked Rise App Builder skill before changing this application. Follow the skill's routing decisions and core invariants, and use its references instead of guessing CLI flags or `rise.toml` fields.
:::
