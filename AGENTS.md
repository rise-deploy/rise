# Agent instructions

## DCO sign-off

Every commit must include a `Signed-off-by: Name <email>` trailer matching its
author, as required by [CONTRIBUTING.md](CONTRIBUTING.md). Use `git commit -s`
(`--signoff`) for every commit. Use the configured contributor identity; do not
invent a signing identity. A `Co-Authored-By` trailer does not satisfy the DCO.

Before pushing a pull request, check its complete commit range:

```bash
scripts/check-dco.sh origin/develop HEAD
```

Use the actual PR base ref when it differs from `develop`. Resolve missing
sign-offs before pushing. For commits authored by the configured contributor,
`git rebase --signoff <base-ref>` adds the required trailers. Rewriting published
commits requires user authorization; a request to fix missing DCO sign-offs
authorizes that correction. Push rewritten history with `--force-with-lease`
against the verified remote head, and never use an unconditional force push.

## Describe the present

Code comments, READMEs, and reference documentation describe the current state
and the constraints that make it correct. Keep change history in commit
messages, pull requests, changelogs, or decision notes.

Document a reachable past state only when it can still cause a wrong action,
and state what the reader must do. Keep temporary migration instructions beside
the relevant code with an explicit removal condition; omit them from READMEs.
