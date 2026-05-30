-- Configurable external feeds for hive-wire-ingest (RSS today; scrape later).
-- Distinct from wire_events rows: this table drives polling, not situational cache.

CREATE TABLE wire_sources (
    id uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
    name text NOT NULL UNIQUE,
    kind text NOT NULL DEFAULT 'rss',
    url text NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    poll_interval_secs integer NOT NULL DEFAULT 3600,
    source_tag text NOT NULL,
    category text,
    affects text,
    default_severity text,
    last_fetched_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_wire_sources_enabled ON wire_sources (enabled) WHERE enabled;
