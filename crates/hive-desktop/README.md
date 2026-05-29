# hive-desktop (parked)

Experimental Slint native client for journal/tasks/notes against hive-api.

Not in the workspace `members` list yet. Slint links under GPLv3, so this crate
uses `GPL-3.0-or-later` while the rest of the hive workspace stays MIT.

Resume checklist before promoting:

- Add to root `Cargo.toml` `members` (and decide workspace vs standalone build).
- Wire auth (device flow or token file) through `hive-core` resolver.
- Born-green: fmt, clippy, manual smoke against local `docker-compose.local.yml`.
