-- AIs aren't people. They share a shape (slug + display_name + notes) but the
-- conceptual collision was muddy: a `kind` column on `people` doesn't describe
-- a property of a human; it discriminates two different entity kinds living in
-- one table. Split them so the shape of `people` says "human" without saying
-- so, and AIs get their own first-class directory.
--
-- The new `ai` table is the AI DIRECTORY (pia/apis/cera). It's independent
-- from `ai_identities` (the auth-side grant table from migration 0006), which
-- discriminates on grant-shape, not directory-shape. Don't conflate them.

CREATE TABLE ai (
  id uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  slug TEXT NOT NULL UNIQUE CHECK (slug ~ '^[a-z][a-z0-9_-]*$'),
  display_name TEXT NOT NULL,
  -- 'assistant' = Claude-flavored helper; 'agent' = autonomous worker;
  -- 'persona' = named character or role. Independent from the auth-side
  -- ai_identities.kind which discriminates on grant-shape, not directory-shape.
  kind TEXT NOT NULL DEFAULT 'assistant' CHECK (kind IN ('assistant','agent','persona')),
  notes TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_ai_slug ON ai(slug);

-- Move the AI directory rows out of people. The kind column on people is
-- dropped after the move so the table shape says "human" without a column
-- saying so.
INSERT INTO ai (slug, display_name, notes)
SELECT slug, display_name, notes FROM people WHERE kind = 'ai';

DELETE FROM people WHERE kind = 'ai';

ALTER TABLE people DROP COLUMN kind;
