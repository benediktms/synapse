DROP TRIGGER memories_fts_after_insert;
DROP TRIGGER memories_fts_after_delete;
DROP TRIGGER memories_fts_after_update;
DROP TABLE memories_fts;

CREATE VIRTUAL TABLE memories_fts USING fts5(
    title, content, content='memories', content_rowid='rowid'
);

CREATE TRIGGER memories_fts_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
END;

CREATE TRIGGER memories_fts_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
END;

CREATE TRIGGER memories_fts_after_update AFTER UPDATE OF title, content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content)
    VALUES ('delete', old.rowid, old.title, old.content);
    INSERT INTO memories_fts(rowid, title, content)
    VALUES (new.rowid, new.title, new.content);
END;

INSERT INTO memories_fts(memories_fts) VALUES ('rebuild');
