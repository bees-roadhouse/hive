---
description: Save a durable Hive journal memory
argument-hint: "<memory prose>"
---

Save the user's supplied prose as a Hive journal memory.

Use the checked-out Hive repo and run:

```bash
pnpm --dir "$HIVE_REPO_PATH" --filter @hive/agent start -- journal-add --tags=session "$ARGUMENTS"
```

If `HIVE_REPO_PATH` is unset, first use the repo that contains `packages/agent`.
Write prose, not terse facts. Preserve concrete names, dates, decisions, and why
the memory should matter later.
