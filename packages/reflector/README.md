# @hive/reflector — the reflection loop

Turns captured conversations into durable memory. The Claude Code plugin's
SessionEnd hook (or any `/api/conversations` producer) stores transcripts of
local agent sessions; the reflector drains that queue for **one AI identity**,
reflects on each transcript with Claude, and writes the result back to hive:

```
GET  /api/conversations/pending        → captured sessions, reflected_at IS NULL
GET  /api/conversations/{id}           → transcript flattened to text
(LLM reflect)                          → summary + narrative + proposed tasks/decisions
POST /api/journal                      → durable memory (anchors only in auto mode)
POST /api/conversations/{id}/reflected → store rolling summary, drain the queue
```

The token decides the identity and the memory namespace: the loop reflects
exactly the conversations that token can see, and everything it writes lands
in that token's owner scope.

Zero dependencies — built-in `fetch`, TypeScript run directly via
`node --experimental-strip-types` (Node 22+). No build step.

## Modes — `REFLECTION_MODE` (default `suggest`)

| Mode | Behaviour |
|---|---|
| `off` | Do nothing; exit immediately. The identity opted out of reflection. |
| `suggest` | Write the journal narrative plus a plain **Proposed follow-ups** prose section, tagged `reflection` + `suggestion`. **No anchors**, so nothing is auto-created — a human reviews and promotes. Rolling summary still stored. |
| `auto` | Additionally anchor the tasks/decisions (UTF-16 span offsets, matching hive's `js_slice_utf16`) so hive materializes them immediately. Tagged `reflection`. |

**Why `suggest` is the default:** reflection runs unattended over transcripts
that can contain untrusted input — including mail, which is new to hive in
0.6.0. An agent that reads a hostile email and then writes anchored tasks has
been handed an instruction-injection → action pipeline. In `suggest` mode the
loop only *proposes* in prose; a human promotes proposals to real
tasks/decisions. Flip to `auto` per identity once you trust the input surface.

## The namespace refusal

Reflection memory must be **private by construction**. At startup the
reflector calls `GET /api/auth/me` with its Bearer token; that route is
public and resolves an invalid/expired token to `{"principal": null}` rather
than a 401. If the principal is null — a namespace-less caller — the
reflector **exits 1 with a clear error and reflects nothing**. A resolving
token principal is namespaced by construction server-side (its namespace is
the granting human's), and the reflector re-asserts that on every write: if a
journal response ever comes back without `user_scope`, it aborts before
marking the conversation reflected. This is the belt; the server's journal
mail-scope guard (agent-authored global `[mail:]` entries are downgraded to
the author's owner scope) is the suspenders.

Practical upshot: mint the token **for the AI identity** (Settings → API
tokens). Don't reuse an admin service token — an admin-acting token sees
*everyone's* pending conversations and journals them into the admin's
namespace.

## Configuration

| Env | Default | Meaning |
|---|---|---|
| `HIVE_API_URL` | `http://localhost:7878` | hive API base (legacy `HIVE_URL` accepted) |
| `HIVE_API_TOKEN` | — (required) | the AI identity's hive PAT (legacy `HIVE_TOKEN` accepted) |
| `ANTHROPIC_API_KEY` | — (required unless mode=off) | Anthropic API key — per-token billing, the surface meant for automation |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | gateway/proxy override (also the test seam) |
| `REFLECTION_MODE` | `suggest` | `off` \| `suggest` \| `auto` |
| `REFLECTION_INTERVAL_SECS` | `300` | poll interval between passes |

### Cost knobs

| Env | Default | Meaning |
|---|---|---|
| `REFLECTION_MODEL` | `claude-sonnet-5` | reflection model |
| `REFLECTION_MAX_CHARS` | `200000` | transcripts over this are **drained without an LLM call** (the skip is noted in the stored summary) |
| `REFLECTION_MAX_TOKENS` | `4096` | Anthropic `max_tokens` per reflection |
| `REFLECTION_BATCH` | `20` | pending conversations fetched per pass |

Cost intuition: one reflection ≈ one Messages call over the transcript, so
spend scales with captured-session volume, bounded by
`REFLECTION_MAX_CHARS × passes`. Idle passes are free (one hive GET).

## Run

Continuous (what the compose service does):

```bash
HIVE_API_URL=... HIVE_API_TOKEN=... ANTHROPIC_API_KEY=... pnpm --filter @hive/reflector start
```

One pass — drain what's pending, then exit (cron, smoke tests):

```bash
node --experimental-strip-types packages/reflector/src/index.ts --once
```

## Compose

The `reflector` service in `docker/docker-compose.rust.yml` sits behind the
`reflector` profile — nothing runs until you opt in:

```bash
HIVE_API_TOKEN=hive_pat_… ANTHROPIC_API_KEY=sk-ant-… \
docker compose -f docker/docker-compose.rust.yml --profile reflector up -d
```

`HIVE_API_TOKEN` and `ANTHROPIC_API_KEY` are `:?`-required by the service;
`REFLECTION_MODE` defaults to `suggest` and `REFLECTION_INTERVAL_SECS` to 300.
Note the runner profile also reads `HIVE_API_TOKEN` — if you run both
profiles from one `.env`, they would share a token, and the runner's is an
admin service PAT. Give the reflector its own deployment (or its own env
file) so it runs as the AI identity, per the namespace section above.
`REFLECTION_MODE=off` makes the process exit immediately, which
`restart: unless-stopped` will loop — to turn reflection off in compose, stop
the profile instead.

## Failure semantics

- A failed reflection (LLM error, bad JSON, hive hiccup) logs and leaves that
  conversation **pending**; the next pass retries it. One bad transcript
  never takes down the drain.
- An empty transcript is drained with an empty summary so it can't loop
  forever; an oversize one is drained with a skip note (see cost knobs).
- A journal write that comes back without an owner scope aborts the process
  (see the namespace refusal above).

Requires hive ≥ 0.6.0 (conversations API).
