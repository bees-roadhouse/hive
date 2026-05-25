# Portainer deploy: hive

How to bring up the `hive` stack (postgres + hive-api) on a Portainer-managed host so laptop, desktop, and iPad clients can hit a single shared database over the LAN.

The stack file lives at [`docker/docker-compose.yml`](../docker/docker-compose.yml). It pulls `ghcr.io/bees-roadhouse/hive-api:latest`, runs `pgvector/pgvector:pg17` (postgres 17 with the pgvector extension preinstalled ... required for the embeddings column), persists data in a named volume `hive-data`, and exposes the API on port `7878`.

## Prereqs

- Portainer-managed Docker (or Podman) host on the LAN.
- The host can reach `ghcr.io` and Docker Hub (for `pgvector/pgvector`). The `bees-roadhouse/hive-api` image is published by the CI workflow on push to main ... if it's still private at deploy time, configure a registry credential in Portainer under **Registries** with a GitHub PAT scoped to `read:packages`.
- An `edge` docker network if you're routing through Traefik/Caddy. Create once on the host: `docker network create edge`. If you don't run a reverse proxy yet, drop the `edge` block from the compose `networks:` section before deploying ... `default` alone is fine.
- A real `POSTGRES_PASSWORD` for production. The compose file defaults to `hive` for local dev. Override it via Portainer's stack env vars before deploying.

## Step 1: deploy the stack with an empty database

Unlike the old sqlite world (where you SCPed `hive.db` into the volume before first boot), postgres initializes its data directory on first start. The named volume `hive-data` should be empty when the stack comes up the first time.

In Portainer:

1. **Stacks** ... **Add stack**.
2. Name it `hive`.
3. Build method: **Web editor** (paste `docker/docker-compose.yml` contents) for the first run. Switch to **Repository** later once the CI workflow is stable and you want stack-from-git auto-updates pointed at `bees-roadhouse/hive` `main`.
4. **Environment variables** ... set `POSTGRES_PASSWORD` to a real secret. Don't ship the dev default to prod.
5. **Deploy the stack**.

Portainer pulls both images, creates the named volume, starts postgres, waits for its healthcheck (`pg_isready`), then starts `hive-api` once postgres is ready.

At this point you have an empty hive db. Schema creation happens automatically on first hive-api boot (the api runs its own migrations against postgres). No data yet.

## Step 2: seed from the legacy sqlite via `hive-migrate`

The migration tool runs from your laptop. It reads the old `~/.hive/hive.db` and writes into the running postgres over the network. Run it once, post-deploy, to bring your journal, tasks, notes, and wire_events forward.

```bash
# from the laptop where ~/.hive/hive.db lives
hive-migrate \
  --sqlite ~/.hive/hive.db \
  --database-url "postgres://hive:<POSTGRES_PASSWORD>@<portainer-host>:5432/hive"
```

A few notes on this:

- **Expose 5432 temporarily for the seed.** The canonical compose keeps postgres on the internal `hive-internal` network only ... not reachable from the LAN. For the seed step, either (a) `docker exec` into the postgres container and run hive-migrate there with a sqlite file you copied in, or (b) add a one-off `ports: ["5432:5432"]` on the postgres service, run the migration, then remove the port mapping and redeploy. Option (b) is what nate usually does.
- **Idempotent enough to retry.** hive-migrate uses `INSERT ... ON CONFLICT DO NOTHING` for journal entries and tasks; rerunning it doesn't duplicate. But the first run is the canonical one ... if it fails halfway, drop and recreate the volume before retrying so you don't end up with partial state.
- **Verify after.** `curl http://<portainer-host>:7878/journal?limit=3` should show your three most recent entries. If the API returns an empty list, the seed didn't land ... check hive-migrate's stderr.

After this, the Portainer stack is canonical. The laptop sqlite becomes a frozen historical snapshot.

## Step 3: environment

Defaults baked into the images:

- `DATABASE_URL=postgres://hive:hive@postgres:5432/hive`
- `HIVE_API_BIND=0.0.0.0:7878`
- `POSTGRES_USER=hive`, `POSTGRES_DB=hive`, `POSTGRES_PASSWORD=hive` (override in prod)

If you need to point hive-api at an external postgres (managed RDS, separate VM, etc.), override `DATABASE_URL` in the Portainer stack UI rather than editing the file ... keeps the canonical compose clean.

## Step 4: verify

From any LAN client:

```bash
curl http://<portainer-host-ip>:7878/healthz
# {"ok":true}
```

The endpoint is `/healthz`. The compose healthcheck uses the same path. The healthcheck won't go green until postgres is reachable AND the api has run its migrations.

Sanity-check the data made it:

```bash
curl http://<portainer-host-ip>:7878/journal?limit=3
```

Should return your three most recent journal entries.

## Step 5: point clients at the host

The CLI (`python ~/.hive/hive.py`) and the leptos web UI (`hive-ui`) both ship the same resolver ... no wrapper, no extra tooling. Set the three env vars below and the client picks the right URL based on the system's DNS search domain.

```
hive client URL resolution (laptop / agent sessions):

  HIVE_API_URL                                    explicit override; wins outright
  HIVE_PUBLIC_URL                                 e.g. https://hive.beesroadhouse.com
  HIVE_PRIVATE_URL                                e.g. http://hive.home.beesroadhouse.com:7878
  HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS  comma-separated, e.g. home.beesroadhouse.com

resolution order:
  1. if HIVE_API_URL is set, use it
  2. else query the system DNS search domain (Get-DnsClient on Windows,
     /etc/resolv.conf on Linux, scutil --dns on macOS)
  3. if any returned domain matches HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS,
     use HIVE_PRIVATE_URL
  4. else use HIVE_PUBLIC_URL
  5. if neither is set, fall back to http://localhost:7878 (rust) or direct DB (python)

LAN-at-the-roadhouse → private URL. Coffee shop → public. Same env vars, one truth.
```

### Client URL resolution

Set the three vars in your shell profile (or User env on Windows) once, and forget about it:

```bash
# laptop / desktop ... POSIX
export HIVE_PUBLIC_URL=https://hive.beesroadhouse.com
export HIVE_PRIVATE_URL=http://hive.home.beesroadhouse.com:7878
export HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS=home.beesroadhouse.com
```

```powershell
# Windows PowerShell (User scope, persists across sessions)
[Environment]::SetEnvironmentVariable("HIVE_PUBLIC_URL", "https://hive.beesroadhouse.com", "User")
[Environment]::SetEnvironmentVariable("HIVE_PRIVATE_URL", "http://hive.home.beesroadhouse.com:7878", "User")
[Environment]::SetEnvironmentVariable("HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS", "home.beesroadhouse.com", "User")
```

`HIVE_API_URL` still works as an explicit override (CI, ephemeral debugging, sub-agent that should pin to one URL). Set it only when you mean it ... otherwise let the resolver do its thing.

On iPad, set the same three values in the client's settings panel.

## Backups

Postgres backups go through `pg_dump`, not a raw cp of the data directory. Pull a logical dump back to a laptop:

```bash
docker exec hive-postgres \
  pg_dump -U hive -d hive --format=custom \
  > hive-$(date +%F).dump
```

Restore into a fresh postgres with `pg_restore`:

```bash
docker exec -i hive-postgres \
  pg_restore -U hive -d hive --clean --if-exists \
  < hive-2026-05-19.dump
```

Cron a nightly dump on the host:

```cron
0 3 * * * docker exec hive-postgres pg_dump -U hive -d hive --format=custom > /var/backups/hive/hive-$(date +\%F).dump
```

Don't try to back up the named volume by tar'ing `/var/lib/docker/volumes/hive-data/_data` while postgres is running ... that produces a torn snapshot. Always go through `pg_dump`.

## Tradeoff: source-of-truth migration

Pre-Portainer, your laptop's `~/.hive/hive.db` was the canonical store (sqlite). After this stack ships, **the Portainer host's postgres database is canonical**. The laptop becomes a client.

Things to discipline yourself on:

- After Step 2 (the hive-migrate seed), **stop writing to the old laptop hive.db**. Every CLI invocation, every skill, every desktop session needs to go through the API. That means `HIVE_API_URL` (or the resolver vars above) is set everywhere and the old `~/.hive/hive.db` is moved aside (rename it to `hive.db.preportainer` so a stray script can't open it).
- If you're offline (no LAN), the clients can't read or write. Decide whether that's acceptable, or whether you want a periodic pull-down to the laptop for read-only offline use.
- Two laptops both writing to local hive.db files after the cutover = split-brain. The API is the only writer.

The one-time migration is cheap. The discipline afterward is the actual cost.
