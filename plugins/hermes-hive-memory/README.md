# Hive Memory for Hermes Agent

Hermes memory-provider plugin for Hive-backed AI memory.

Install this directory as a Hermes memory provider:

```bash
mkdir -p ~/.hermes/plugins/memory
cp -r /path/to/hive/plugins/hermes-hive-memory ~/.hermes/plugins/memory/hive
hermes memory setup
```

Then select `hive` as the active memory provider.

Required environment:

```bash
HIVE_API_URL=https://hive.example.com
HIVE_API_TOKEN=hive_pat_...
HIVE_IDENTITY=pia
HIVE_PEER=nate
```

What it does:

- injects a Hive session memory block through `system_prompt_block()`;
- uses `/api/recall` for high-confidence semantic recall;
- mirrors Hermes built-in memory writes to `/api/journal`;
- exposes `hive_journal_add`, `hive_recall`, and `hive_search` tools.

Hive remains the source of truth. Do not keep durable AI memory in Hermes-local
`MEMORY.md` as a parallel system.
