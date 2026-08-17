export function buildAgentPrompt(docsBase) {
  return `You are working on an application deployed with Rise.

AUTHORITATIVE SOURCES
1. Read ${docsBase}/llms.txt to discover the available Rise documentation.
2. Read ${docsBase}/skills/rise-app-builder/SKILL.md before making changes.
3. Load the skill's CLI and rise.toml reference files when they apply to the task.

OPERATING RULES
- Treat the hosted skill and documentation as authoritative over prior knowledge.
- Follow the skill's routing decisions and core invariants.
- Inspect the existing application before changing configuration or deployment files.
- Do not guess Rise CLI flags, rise.toml fields, authentication flows, or deployment defaults.

TASK
<Describe the application change or deployment outcome you want.>`;
}
