# hive conventions

Canonical vocabulary and rules for how tasks, journal entries, and links fit together. If something here disagrees with code, code wins ... open a PR to fix the doc.

## link_type vocabulary

Six standard values. Anything else is a custom one-off and should probably be promoted here or replaced with one of these.

| link_type    | direction          | meaning |
|--------------|--------------------|---------|
| `spawned_in` | entry → task       | task was created from inside this entry. one-time creation event. |
| `closed_by`  | entry → task       | entry marks the task done. paired with `tasks.status='done'`. |
| `mentions`   | any → any          | reference without lifecycle. just a pointer. |
| `parent_of`  | any → any          | explicit hierarchy, parent side. |
| `child_of`   | any → any          | explicit hierarchy, child side. used for task-to-non-project parents. |
| `relates_to` | any → any          | generic catchall when nothing more specific fits. |
| `inline_in`  | entry → task       | task is checkbox-embedded in this entry's body. persists for as long as the body still contains the checkbox. distinct from `spawned_in`, which is the creation event only. |

`spawned_in` fires once at birth. `inline_in` is a living binding ... if the checkbox stays, the link stays.

## note spawn blocks

Notes use triple-bracket blocks. Tasks use checkboxes only. Inline `#tag` is folksonomy, not spawn syntax.

```text
[[[note dinner plans project:home tags:food]]]
Reservations at 7pm.
[[[/note]]]
```

- Opener: `[[[note TITLE …]]]` on one line. Optional `project:` and `tags:` tokens on the opener.
- Closer: `[[[/note]]]` on its own line.
- Projection creates a `notes` row and a `spawned_in` link (journal entry → note).

## obsidian-style checklist syntax

Canonical journal embedding. The renderer recognizes:

```
- [ ] open task
- [x] done task
- [-] dropped task (strikethrough at render)
- [/] in-progress task
```

Rules:

- Optional anchor: `- [ ] text ^tasks-123` pins the binding to a specific task id. survives title edits, body reshuffles, anything.
- Without anchor: app fuzzy-matches the checkbox title against existing open tasks at save time. on a match it binds, otherwise it creates a new task and binds.
- The binding lives in the `links` table with `link_type='inline_in'` ... it does NOT live inline in the entry body. body text holds the human-readable checkbox; the link table holds the structural relation.
- Render maps from `task.status`, not from the checkbox character. so when status flips anywhere (CLI, UI, API, another entry), every embedded checkbox in every entry re-renders to match. one source of truth.

## task parent resolution

A task can have at most one project parent ... that lives in the `tasks.project` column. The column is nullable; a task can stand alone with no project.

For any other parent kind (a journal entry, a wire event, another task, a note), use a link:

```
link(source_table='tasks', source_id=T, target_table=<table>, target_id=N, link_type='child_of')
```

Multiple `child_of` links per task are fine. `tasks.project` and `child_of` links are independent ... a task can have both, neither, or either.

## tasks-from-journal lifecycle

| event                                   | write |
|-----------------------------------------|-------|
| task created from inside an entry       | `spawned_in` link (entry → task) |
| entry marks a task done                 | `closed_by` link (entry → task) + `UPDATE tasks SET status='done', closed_at=...` |
| task body checkbox renders in entry     | `inline_in` link (entry → task), if not already present |
| task dropped                            | `UPDATE tasks SET status='dropped'`. do NOT delete `inline_in` links or rewrite old entry bodies. the renderer will apply strikethrough based on status. |

Old entries are immutable history. Status changes propagate through the renderer, not through retroactive edits.
