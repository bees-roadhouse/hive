-- Seed data for the importer's fixture database (PR 1.7): a miniature but
-- representative hosted instance. Reserved fictional values throughout.
--
-- Contents: 2 actors, 3 custom entity types (one with fields + an instance),
-- 9 entities across the built-in kinds (incl. a decision-supersedes pair),
-- 5 journal entries with anchors + mentions + tags, links, a profile card,
-- config kv (incl. app.version, which must NOT migrate), an identity
-- artifact, a platform identity, a feed source, 2 inbox rows (1 unread +
-- 1 read; only the unread one migrates), and a mail account + mailbox +
-- 2 messages + 1 attachment with real BYTEA whose legacy hash is a sha256
-- (NOT the blake3 of the bytes → exercises the re-key alias path). Rows in
-- wire/outbox/search/cc_credentials exist to prove the importer ignores
-- them.

-- ── actors ───────────────────────────────────────────────────────────────────
INSERT INTO people (id, slug, name, kind, owner, bio, role, created_at) VALUES
  ('per_fixnate0001', 'nate', 'Nate', 'human', NULL, 'Keeper of the hive', 'owner', '2025-09-01T08:00:00.000Z'),
  ('per_fixpia00001', 'pia', 'Pia', 'ai', 'nate', NULL, 'resident ai', '2025-09-01T08:00:05.000Z');

INSERT INTO profile (actor, kind, display_name, body, source, derived_at, updated_at) VALUES
  ('nate', 'human', 'Nate', '{"summary":"Owner of this instance","pronouns":"he/him"}', 'manual', NULL, '2025-09-02T09:00:00.000Z');

INSERT INTO identities (id, platform, platform_id, actor, created_at) VALUES
  ('idm_fixdisc0001', 'discord', '110000000000000001', 'nate', '2025-09-03T10:00:00.000Z');

INSERT INTO identity_artifacts (id, actor, kind, name, content, description, enabled, created_at, updated_at) VALUES
  ('iart_fixskill01', 'pia', 'skill', 'journaling', E'# Journaling\nWrite it down before it evaporates.', 'daily journaling skill', TRUE, '2025-09-04T11:00:00.000Z', '2025-09-04T11:00:00.000Z');

INSERT INTO sources (id, name, url, kind, category, severity, interval_secs, notify, enabled, owner, last_polled_at, last_status, created_at) VALUES
  ('src_fixfeed0001', 'Sourdough Digest', 'https://feeds.example.com/sourdough.xml', 'rss', 'baking', 'info', 900, 'nate', TRUE, 'nate', '2025-11-01T06:00:00.000Z', 'ok', '2025-09-05T12:00:00.000Z');

INSERT INTO config (key, value, updated_at) VALUES
  ('app.version', '0.6.0', '2025-09-01T08:00:00.000Z'),
  ('instance.name', 'Bierly-Smith Hive', '2025-09-06T13:00:00.000Z'),
  ('digest.hour', '7', '2025-09-07T14:00:00.000Z');

-- ── custom entity types ──────────────────────────────────────────────────────
INSERT INTO entity_types (id, slug, name, name_plural, description, icon, color, board_field, archived, created_by, created_at, updated_at) VALUES
  ('etype_fixrec001', 'recipe', 'Recipe', 'Recipes', 'Things we bake', 'chef-hat', '#c96f2b', 'stage', FALSE, 'nate', '2025-09-10T09:00:00.000Z', '2025-09-10T09:00:00.000Z'),
  ('etype_fixplant1', 'plant', 'Plant', 'Plants', 'The garden roster', 'leaf', '#3f7a3f', NULL, FALSE, 'nate', '2025-09-10T09:05:00.000Z', '2025-09-10T09:05:00.000Z'),
  ('etype_fixgadget', 'gadget', 'Gadget', 'Gadgets', 'Serial numbers and fates', 'cpu', '#557', NULL, TRUE, 'nate', '2025-09-10T09:10:00.000Z', '2025-10-01T09:10:00.000Z');

INSERT INTO entity_fields (id, type_id, slug, label, field_type, required, position, options, ref_kind, archived, created_at, updated_at) VALUES
  ('efield_fixnote1', 'etype_fixrec001', 'notes', 'Notes', 'text', FALSE, 0, '[]', NULL, FALSE, '2025-09-10T09:01:00.000Z', '2025-09-10T09:01:00.000Z'),
  ('efield_fixstage', 'etype_fixrec001', 'stage', 'Stage', 'choice', TRUE, 1, '["idea","testing","keeper"]', NULL, FALSE, '2025-09-10T09:02:00.000Z', '2025-09-10T09:02:00.000Z'),
  ('efield_fixwater', 'etype_fixplant1', 'watering', 'Watering', 'text', FALSE, 0, '[]', NULL, FALSE, '2025-09-10T09:06:00.000Z', '2025-09-10T09:06:00.000Z');

-- ── built-in entities ────────────────────────────────────────────────────────
INSERT INTO projects (id, name, slug, created_at) VALUES
  ('proj_fixhomest1', 'Homestead', 'homestead', '2025-09-11T08:00:00.000Z');

INSERT INTO topics (id, name, slug, created_at) VALUES
  ('top_fixsourdou1', 'Sourdough', 'sourdough', '2025-09-11T08:30:00.000Z');

INSERT INTO phases (id, project, name, position, created_at) VALUES
  ('ph_fixautumn001', 'proj_fixhomest1', 'Autumn prep', 0, '2025-09-11T09:00:00.000Z');

INSERT INTO tasks (id, project, phase, due, title, body, status, priority, tags, assignees, origin_entry_id, anchor_text, created_at, updated_at) VALUES
  ('task_fixlevain1', 'proj_fixhomest1', 'ph_fixautumn001', '2025-11-20T00:00:00.000Z', 'Refresh the levain schedule', 'Refresh the levain twice daily until the oven repair lands', 'doing', 'high', '["baking"]', '["nate","pia"]', 'jrnl_fixentry01', 'Refresh the levain twice daily', '2025-11-02T10:05:00.000Z', '2025-11-06T18:00:00.000Z'),
  ('task_fixfence01', 'proj_fixhomest1', NULL, NULL, 'Mend the east fence', '', 'todo', 'normal', '[]', '["nate"]', NULL, NULL, '2025-11-03T09:00:00.000Z', '2025-11-03T09:00:00.000Z');

INSERT INTO decisions (id, title, context, decision, consequences, status, tags, assignees, project, supersedes, origin_entry_id, anchor_text, created_at, updated_at) VALUES
  ('dec_fixoven0001', 'Repair the deck oven', 'The deck oven trips its breaker on preheat', 'Repair the existing deck oven', 'Bread nights continue uninterrupted', 'superseded', '["kitchen"]', '["nate"]', 'proj_fixhomest1', NULL, NULL, NULL, '2025-10-15T12:00:00.000Z', '2025-11-04T12:00:00.000Z'),
  ('dec_fixoven0002', 'Replace the deck oven', 'Repair quote came back above replacement cost', 'Replace the deck oven with a refurbished unit', 'Two bread-less weekends while it ships', 'accepted', '["kitchen"]', '["nate"]', 'proj_fixhomest1', 'dec_fixoven0001', 'jrnl_fixentry02', 'replace the deck oven outright', '2025-11-04T12:00:00.000Z', '2025-11-04T12:00:00.000Z');

INSERT INTO events (id, title, body, at, tags, assignees, origin_entry_id, anchor_text, created_at) VALUES
  ('evt_fixmarket01', 'Winter market stall', 'Stall booked for the winter market opening weekend', '2025-12-06T08:00:00.000Z', '["market"]', '["nate"]', 'jrnl_fixentry03', 'Stall booked for the winter market', '2025-11-05T15:00:00.000Z');

INSERT INTO entities (id, type_id, title, fields, user_scope, origin_entry_id, created_by, created_at, updated_at) VALUES
  ('ent_fixrecipe01', 'etype_fixrec001', 'Overnight country loaf', '{"notes": "Slow ferment overnight, bake at 245C", "stage": "keeper"}'::jsonb, NULL, NULL, 'nate', '2025-11-01T20:00:00.000Z', '2025-11-06T20:00:00.000Z');

-- ── journal + anchors ────────────────────────────────────────────────────────
INSERT INTO journal (id, author, body, tags, mentions, user_scope, created_at) VALUES
  ('jrnl_fixentry01', 'nate', 'Refresh the levain twice daily until the oven repair lands. @pia keep me honest on the afternoon feed.', '["baking"]', '["pia"]', NULL, '2025-11-02T10:05:00.000Z'),
  ('jrnl_fixentry02', 'nate', 'Quote for the oven repair came in absurd, so we will replace the deck oven outright.', '["kitchen"]', '[]', NULL, '2025-11-04T12:00:00.000Z'),
  ('jrnl_fixentry03', 'nate', 'Stall booked for the winter market opening weekend. The zibaldone pays for itself again.', '["market"]', '[]', NULL, '2025-11-05T15:00:00.000Z'),
  ('jrnl_fixentry04', 'pia', 'Filed the [topic: Sourdough] notes from this week under the homestead project.', '["baking","notes"]', '[]', NULL, '2025-11-06T09:00:00.000Z'),
  ('jrnl_fixentry05', 'nate', 'Private scratchpad line about the surprise gift budget.', '[]', '[]', 'nate', '2025-11-07T21:30:00.000Z');

INSERT INTO anchors (id, entry_id, start, "end", text, kind, ref_id, created_at) VALUES
  ('anc_fixlevain01', 'jrnl_fixentry01', 0, 30, 'Refresh the levain twice daily', 'task', 'task_fixlevain1', '2025-11-02T10:05:00.000Z'),
  ('anc_fixoven0001', 'jrnl_fixentry02', 55, 86, 'replace the deck oven outright', 'decision', 'dec_fixoven0002', '2025-11-04T12:00:00.000Z'),
  ('anc_fixmarket01', 'jrnl_fixentry03', 0, 34, 'Stall booked for the winter market', 'event', 'evt_fixmarket01', '2025-11-05T15:00:00.000Z');

-- ── links ────────────────────────────────────────────────────────────────────
INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) VALUES
  ('link_fixanch001', 'journal', 'jrnl_fixentry01', 'task', 'task_fixlevain1', 'anchors', '2025-11-02T10:05:00.000Z'),
  ('link_fixanch002', 'journal', 'jrnl_fixentry02', 'decision', 'dec_fixoven0002', 'anchors', '2025-11-04T12:00:00.000Z'),
  ('link_fixanch003', 'journal', 'jrnl_fixentry03', 'event', 'evt_fixmarket01', 'anchors', '2025-11-05T15:00:00.000Z'),
  ('link_fixsuper01', 'decision', 'dec_fixoven0002', 'decision', 'dec_fixoven0001', 'supersedes', '2025-11-04T12:00:00.000Z'),
  ('link_fixtag0001', 'journal', 'jrnl_fixentry04', 'topic', 'top_fixsourdou1', 'tagged', '2025-11-06T09:00:00.000Z'),
  ('link_fixrec0001', 'recipe', 'ent_fixrecipe01', 'topic', 'top_fixsourdou1', 'relates', '2025-11-06T20:00:00.000Z');

-- ── inbox (1 unread migrates, 1 read stays behind) ───────────────────────────
INSERT INTO inbox (id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at) VALUES
  ('inb_fixunread01', 'pia', 'nate', 'assignment', 'task', 'task_fixlevain1', 'jrnl_fixentry01', 'Refresh the levain twice daily', '2025-11-02T10:05:00.000Z', NULL),
  ('inb_fixread0001', 'nate', 'pia', 'mention', 'journal', 'jrnl_fixentry04', 'jrnl_fixentry04', 'Filed the sourdough notes', '2025-11-06T09:00:00.000Z', '2025-11-06T09:30:00.000Z');

-- ── mail: account (with a credential that must NOT migrate) + cursor ─────────
INSERT INTO cc_credentials (id, owner, kind, runtime, provider, label, ciphertext, nonce, tail, created_at, last_used_at) VALUES
  ('cred_fixmail001', 'nate', 'mail_password', 'mail', 'fastmail', 'nate@example.com', 'AAAA-not-a-real-ciphertext', 'AAAA-nonce', '…7x', '2025-09-20T08:00:00.000Z', '2025-11-01T08:00:00.000Z');

INSERT INTO mail_accounts (id, owner, address, jmap_url, jmap_username, jmap_account_id, cred_id, email_state, mailbox_state, backfill_status, backfill_cursor, attempts, next_attempt_at, last_error, last_synced_at, last_status, enabled, created_at, updated_at) VALUES
  ('macct_fixnate01', 'nate', 'nate@example.com', 'https://jmap.example.com/session', 'nate@example.com', 'acc-jmap-01', 'cred_fixmail001', 'es-000042', 'ms-000017', 'complete', '{"upTo": "2025-10-01T00:00:00Z", "window": 500}'::jsonb, 0, NULL, NULL, '2025-11-07T06:00:00.000Z', 'ok', TRUE, '2025-09-20T08:00:00.000Z', '2025-11-07T06:00:00.000Z');

INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, role, ingest, sort_order) VALUES
  ('mbox_fixinbox01', 'macct_fixnate01', 'mb-jmap-inbox', 'Inbox', 'inbox', TRUE, 0);

-- Message 1: clean, in the ingest mailbox → searchable + embed 'pending'.
-- Message 2: $junk-flagged → embed 'skip', no FTS row; carries the attachment.
INSERT INTO mail_messages (id, account_id, jmap_id, jmap_thread_id, message_id_hdr, in_reply_to, references_json, from_addr, from_name, to_json, cc_json, reply_to_json, subject, sent_at, received_at, mailbox_ids_json, keywords_json, body_text, body_source, snippet, size, has_attachments, embed_state, user_scope, deleted_at, created_at, updated_at) VALUES
  ('mail_fixmsg0001', 'macct_fixnate01', 'em-jmap-0001', 'th-jmap-0001', '<millwheel-invoice@example.com>', NULL, '[]', 'miller@example.com', 'The Miller', '["nate@example.com"]', '[]', '[]', 'Millwheel flour invoice', '2025-10-30T07:58:00.000Z', '2025-10-30T08:00:00.000Z', '["mb-jmap-inbox"]', '{"$seen": true}', 'Invoice attached for the autumn millwheel flour order, twenty sacks.', 'plain', 'Invoice attached for the autumn millwheel flour order…', 2048, FALSE, 'done', 'nate', NULL, '2025-10-30T08:00:00.000Z', '2025-10-30T08:00:00.000Z'),
  ('mail_fixmsg0002', 'macct_fixnate01', 'em-jmap-0002', 'th-jmap-0002', '<prize-draw@example.net>', NULL, '[]', 'promo@example.net', NULL, '["nate@example.com"]', '[]', '[]', 'You may already be a winner', '2025-11-01T11:00:00.000Z', '2025-11-01T11:01:00.000Z', '["mb-jmap-inbox"]', '{"$seen": false, "$junk": true}', 'Claim your prize draw entry before midnight.', 'html', 'Claim your prize draw entry…', 4096, TRUE, 'skip', 'nate', NULL, '2025-11-01T11:01:00.000Z', '2025-11-01T11:01:00.000Z');

-- Attachment bytes: 50 bytes, sha256-keyed in the legacy blobs table — the
-- importer re-keys to blake3(bytes) and emits an `alias` record.
INSERT INTO blobs (hash, size, mime, data, created_at) VALUES
  ('b15b6a39a8ca5340b09cdd0af135e7d495b602a220cf1ee1c21593ea8336e577', 50, 'application/pdf', '\x686976652066697874757265206174746163686d656e742062797465732076313a20255044462d312e34206d696e696d616c', '2025-11-01T11:02:00.000Z');

INSERT INTO mail_attachments (id, message_id, blob_hash, jmap_blob_id, filename, mime, size, content_id, disposition, skipped_reason, created_at) VALUES
  ('matt_fixpdf0001', 'mail_fixmsg0002', 'b15b6a39a8ca5340b09cdd0af135e7d495b602a220cf1ee1c21593ea8336e577', 'bl-jmap-0001', 'prize-rules.pdf', 'application/pdf', 50, NULL, 'attachment', NULL, '2025-11-01T11:02:00.000Z');

-- ── rows the importer must IGNORE (derived / transient / hosted-era) ─────────
INSERT INTO wire (id, kind, actor, payload, created_at) VALUES
  ('wire_fixevent01', 'journal.created', 'nate', '{"id":"jrnl_fixentry01"}', '2025-11-02T10:05:00.000Z');
INSERT INTO outbox (id, kind, payload, status, attempts, last_error, run_after, created_at, completed_at) VALUES
  ('out_fixwebhook1', 'webhook', '{"url":"https://hooks.example.com/x"}', 'pending', 0, NULL, '2025-11-08T00:00:00.000Z', '2025-11-07T23:00:00.000Z', NULL);
INSERT INTO search (kind, ref_id, title, body) VALUES
  ('journal', 'jrnl_fixentry01', 'nate: Refresh the levain twice daily…', 'Refresh the levain twice daily until the oven repair lands baking');
INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, vec_v, hash, created_at) VALUES
  ('journal', 'jrnl_fixentry01', 0, 'hash-256', 256, NULL, '\x00000000', NULL, 'stale-hash', '2025-11-02T10:06:00.000Z');
INSERT INTO worker_status (id, heartbeat, last_run) VALUES
  (1, '2025-11-07T23:50:00.000Z', '{"sources":1}');
