-- task_anchors: tie an Obsidian-style ^taskN block id on a journal entry to a tasks row.
CREATE TABLE task_anchors (
  task_id BIGINT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  journal_entry_id BIGINT NOT NULL REFERENCES journal_entries(id) ON DELETE CASCADE,
  block_id TEXT NOT NULL CHECK (block_id ~ '^[a-z][a-z0-9_-]*$'),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (journal_entry_id, block_id)
);
CREATE INDEX task_anchors_task_id_idx ON task_anchors(task_id);

-- people: first-class person records (ai or human) referenced by @mentions in journal entries.
CREATE TABLE people (
  id BIGSERIAL PRIMARY KEY,
  slug TEXT NOT NULL UNIQUE CHECK (slug ~ '^[a-z][a-z0-9_-]*$'),
  display_name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('ai', 'human')),
  notes TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO people (slug, display_name, kind, notes) VALUES
  ('pia', 'Pia (Apiara)', 'ai', 'Assistant to the CTO + Maggie. Personal-context, calendar, inbox, household. Runs from ~/.claude/.'),
  ('apis', 'Apis', 'ai', 'VP of AI Development. DTC-side code + MSP ops. Runs from ~/.claude-apis-dtc/.'),
  ('cera', 'Cera', 'ai', 'VP of Technology for Bee''s Roadhouse. BR infrastructure + repos. Runs from ~/.claude-cera/.'),
  ('nate', 'Nate Smith', 'human', 'Owner / principal. CTO of DTC. nate@beesroadhouse.com.'),
  ('maggie', 'Maggie Bierly', 'human', 'Co-principal in Pia''s config. Nate''s wife.');
