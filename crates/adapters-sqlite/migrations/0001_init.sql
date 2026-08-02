CREATE TABLE memories (
    id              TEXT PRIMARY KEY NOT NULL,
    content         TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('user','feedback','project','reference')),
    scope           TEXT NOT NULL,
    tags            TEXT NOT NULL DEFAULT '[]',
    pinned          INTEGER NOT NULL DEFAULT 0,
    embedding       BLOB NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    embedding_model TEXT NOT NULL,
    embedding_dim   INTEGER NOT NULL
);

CREATE VIRTUAL TABLE memories_fts USING fts5(
    content, content='memories', content_rowid='rowid'
);

CREATE TRIGGER memories_fts_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER memories_fts_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER memories_fts_after_update AFTER UPDATE OF content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
