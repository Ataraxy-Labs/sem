export const SCHEMA_DDL = `
CREATE TABLE IF NOT EXISTS entities (
  id TEXT PRIMARY KEY,
  file_path TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  name TEXT NOT NULL,
  parent_id TEXT,
  content TEXT,
  content_hash TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  commit_sha TEXT,
  snapshot TEXT NOT NULL DEFAULT 'current'
);

CREATE INDEX IF NOT EXISTS idx_entities_file ON entities(file_path);
CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(entity_type);
CREATE INDEX IF NOT EXISTS idx_entities_snapshot ON entities(snapshot);
CREATE INDEX IF NOT EXISTS idx_entities_hash ON entities(content_hash);

CREATE TABLE IF NOT EXISTS changes (
  id TEXT PRIMARY KEY,
  entity_id TEXT NOT NULL,
  change_type TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  entity_name TEXT NOT NULL,
  file_path TEXT NOT NULL,
  old_file_path TEXT,
  before_content TEXT,
  after_content TEXT,
  commit_sha TEXT,
  author TEXT,
  timestamp TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_changes_file ON changes(file_path);
CREATE INDEX IF NOT EXISTS idx_changes_type ON changes(change_type);
CREATE INDEX IF NOT EXISTS idx_changes_entity_type ON changes(entity_type);
CREATE INDEX IF NOT EXISTS idx_changes_commit ON changes(commit_sha);

CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS labels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_id TEXT NOT NULL,
  label TEXT NOT NULL,
  created_at TEXT DEFAULT (datetime('now')),
  UNIQUE(entity_id, label)
);

CREATE INDEX IF NOT EXISTS idx_labels_entity ON labels(entity_id);
CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);

CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_id TEXT NOT NULL,
  author TEXT,
  body TEXT NOT NULL,
  parent_id INTEGER,
  created_at TEXT DEFAULT (datetime('now')),
  FOREIGN KEY (parent_id) REFERENCES comments(id)
);

CREATE INDEX IF NOT EXISTS idx_comments_entity ON comments(entity_id);
`;
