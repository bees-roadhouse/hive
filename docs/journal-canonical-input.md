# Journal-canonical input

Structured hive state (tasks, notes, links, graph edges) **emerges from journal
prose** as it is written. The journal is the only human/AI content input surface.
Everything projected from it is **read-only via the API** ... mutations flow
through new journal entries, not direct row writes.

**Wire** is the sole exception: external situational feed + future shared
messaging bridge. It does not emerge from journal bodies.

## Write surfaces

| Surface | POST (input) | Read | Role |
|---------|--------------|------|------|
| `POST /journal` | yes | GET | canonical input; triggers projection |
| `POST /wire` | yes (worker/agents) | GET | external events (CVE, RSS, outages) |
| `POST /wire/{id}/ack` | yes | — | operator ack on wire row |
| `/wire/sources` | yes (CRUD) | GET | feed config for wire-ingest worker |
| `tasks`, `notes`, `links`, `events`, `messages`, `projects` | **no** (enforce mode) | GET | projected / legacy read models |

Auth, OAuth, MFA, and admin routes are unchanged.

## Projection pipeline

On `POST /journal` (and on `PATCH /journal/{id}` re-projection):

1. Assign stable `^taskN` block ids to checkbox lines missing anchors.
2. Parse body (`hive-md`): entity mentions + inline tasks + `#note` spawn blocks.
3. Resolve mentions → `links` rows (`link_type='mentions'`).
4. For each checkbox with a block id:
   - Reuse task by title match, or create (respecting `proj:` on the line).
   - Write `spawned_in` (on create), `inline_in`, and `task_anchors`.
   - Checked `- [x]` → `tasks.status=done` + `closed_by` link.
   - Dropped `- [-]` → `tasks.status=dropped`.
5. For each `#note title` block → create `notes` row + `spawned_in` link.

### What emerges from journal vs what does not

| Entity | Created from journal? | How |
|--------|----------------------|-----|
| **Tasks** | yes | Inline `- [ ]` / `- [x]` / `- [-]` checkboxes; `proj:`, `@owner`, `due:`, `pri:` tokens |
| **Notes** | yes | `#note title` blocks (optional `project:` / `tags:` on header line) |
| **Links** | yes | `@slug`, `[[type:id]]`, `[[slug]]` mentions → `link_type='mentions'`; task lifecycle links (`spawned_in`, `inline_in`, `closed_by`) from checkboxes |
| **People / AI** | **no** (link only) | `@slug` resolves to existing `people` or `ai` rows; does not create humans or agents |
| **Wire events** | **no** (link only) | `[[event:…]]` resolves to existing `wire_events`; new events come from **wire ingest** (`POST /wire`, RSS worker) |
| **Projects** | **no** | Still configured directly (or via migration); inline tasks reference projects by name on the checkbox line |

People and wire events are **reference targets** in journal prose, not journal-spawned rows. Wire remains the external input surface.

## Backend / CLI / MCP callers

Agents, CLI commands, and MCP tools that today call `POST /tasks` or
`POST /notes` must **synthesize journal entries** instead:

```text
POST /journal
{
  "ai": "pia",
  "title": "task: fix traefik timeout",
  "body": "- [ ] fix traefik timeout\n\n[[task:...|fix-traefik]]",
  "tags": "backend-input"
}
```

The projection layer creates or binds the task. The entry is durable audit
trail ... same as if Nate typed it.

CLI migration: `hive tasks add` → journal synthesis when `HIVE_INPUT_MODE=enforce`
(follow-up).

## Wire subsystem

### Ingest (today)

- **`hive-wire-ingest`** worker polls `wire_sources` (RSS URLs), inserts rows
  via `wire_events` dedupe on `external_id`.
- Legacy **`watch-the-wire`** skill and other push clients may still
  `POST /wire` directly.

### Config

`wire_sources` rows: name, url, poll interval, `source` tag (maps to
`wire_events.source`), optional category/affects/severity defaults.

Configured via UI (future), `hive wire sources …` CLI, or MCP.

### Messaging bridge (future)

Shared journal entries / tasks / notes between users and AIs will arrive on
**wire**, not as direct cross-principal writes. Design TBD; schema reserves
`wire_events` + `messages` for that seam.

## Enforcement

`HIVE_INPUT_MODE` on hive-api:

| Value | Behavior |
|-------|----------|
| `legacy` (default) | All write routes remain (CLI parity). |
| `shadow` | Log blocked structured writes; still allow. |
| `enforce` | Return 403 on direct structured writes; journal + wire only. |

Shadow first, then flip to enforce once CLI/MCP synthesize journal entries.

## Gaps vs conventions.md

Tracked follow-ups:

- Anchor syntax `^tasks-{uuid}` binding (today: title match + `^taskN` block ids).
- Journal PATCH + re-projection on edit.
- `hive-cli` journal synthesis for tasks/notes/links commands.

See [conventions.md](./conventions.md) for link_type vocabulary and renderer rules.
