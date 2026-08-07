-- Forget deletes from `memories` directly, so a memory that has links otherwise
-- fails with a foreign-key constraint. Rebuild `links` with ON DELETE CASCADE so
-- removing either endpoint removes the edge.
CREATE TABLE links_new (
    low_id     TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    high_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL CHECK (relation IN ('relation','support','contradiction','supersession')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (low_id, high_id, relation)
);

INSERT INTO links_new (low_id, high_id, relation, created_at)
    SELECT low_id, high_id, relation, created_at FROM links;

DROP TABLE links;
ALTER TABLE links_new RENAME TO links;

CREATE INDEX links_low  ON links(low_id);
CREATE INDEX links_high ON links(high_id);
