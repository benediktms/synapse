CREATE TABLE links (
    low_id     TEXT NOT NULL REFERENCES memories(id),
    high_id    TEXT NOT NULL REFERENCES memories(id),
    relation   TEXT NOT NULL CHECK (relation IN ('relation','support','contradiction','supersession')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (low_id, high_id, relation)
);

CREATE INDEX links_low  ON links(low_id);
CREATE INDEX links_high ON links(high_id);
