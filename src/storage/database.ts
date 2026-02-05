import Database from 'better-sqlite3';
import type { SemanticEntity } from '../model/entity.js';
import type { SemanticChange } from '../model/change.js';
import { SCHEMA_DDL } from './schema.js';

export class SemDatabase {
  private db: Database.Database;

  constructor(dbPath: string) {
    this.db = new Database(dbPath);
    this.db.pragma('journal_mode = WAL');
    this.db.pragma('synchronous = NORMAL');
    this.init();
  }

  private init(): void {
    this.db.exec(SCHEMA_DDL);
  }

  setMetadata(key: string, value: string): void {
    this.db.prepare(
      'INSERT OR REPLACE INTO metadata (key, value) VALUES (?, ?)'
    ).run(key, value);
  }

  getMetadata(key: string): string | undefined {
    const row = this.db.prepare('SELECT value FROM metadata WHERE key = ?').get(key) as { value: string } | undefined;
    return row?.value;
  }

  insertEntities(entities: SemanticEntity[], snapshot: string = 'current', commitSha?: string): void {
    const insert = this.db.prepare(`
      INSERT OR REPLACE INTO entities (id, file_path, entity_type, name, parent_id, content, content_hash, start_line, end_line, commit_sha, snapshot)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);

    const tx = this.db.transaction((ents: SemanticEntity[]) => {
      for (const e of ents) {
        insert.run(e.id, e.filePath, e.entityType, e.name, e.parentId ?? null, e.content, e.contentHash, e.startLine, e.endLine, commitSha ?? null, snapshot);
      }
    });

    tx(entities);
  }

  getEntities(snapshot: string = 'current', filePath?: string): SemanticEntity[] {
    let sql = 'SELECT * FROM entities WHERE snapshot = ?';
    const params: unknown[] = [snapshot];

    if (filePath) {
      sql += ' AND file_path = ?';
      params.push(filePath);
    }

    const rows = this.db.prepare(sql).all(...params) as Array<Record<string, unknown>>;
    return rows.map(row => ({
      id: row.id as string,
      filePath: row.file_path as string,
      entityType: row.entity_type as string,
      name: row.name as string,
      parentId: (row.parent_id as string) || undefined,
      content: row.content as string,
      contentHash: row.content_hash as string,
      startLine: row.start_line as number,
      endLine: row.end_line as number,
    }));
  }

  clearSnapshot(snapshot: string): void {
    this.db.prepare('DELETE FROM entities WHERE snapshot = ?').run(snapshot);
  }

  insertChanges(changes: SemanticChange[]): void {
    const insert = this.db.prepare(`
      INSERT OR REPLACE INTO changes (id, entity_id, change_type, entity_type, entity_name, file_path, old_file_path, before_content, after_content, commit_sha, author, timestamp)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
    `);

    const tx = this.db.transaction((chs: SemanticChange[]) => {
      for (const c of chs) {
        insert.run(c.id, c.entityId, c.changeType, c.entityType, c.entityName, c.filePath, c.oldFilePath ?? null, c.beforeContent ?? null, c.afterContent ?? null, c.commitSha ?? null, c.author ?? null);
      }
    });

    tx(changes);
  }

  getChanges(opts?: { filePath?: string; changeType?: string; entityType?: string; commitSha?: string; limit?: number }): SemanticChange[] {
    let sql = 'SELECT * FROM changes WHERE 1=1';
    const params: unknown[] = [];

    if (opts?.filePath) {
      sql += ' AND file_path = ?';
      params.push(opts.filePath);
    }
    if (opts?.changeType) {
      sql += ' AND change_type = ?';
      params.push(opts.changeType);
    }
    if (opts?.entityType) {
      sql += ' AND entity_type = ?';
      params.push(opts.entityType);
    }
    if (opts?.commitSha) {
      sql += ' AND commit_sha = ?';
      params.push(opts.commitSha);
    }

    sql += ' ORDER BY timestamp DESC';

    if (opts?.limit) {
      sql += ' LIMIT ?';
      params.push(opts.limit);
    }

    const rows = this.db.prepare(sql).all(...params) as Array<Record<string, unknown>>;
    return rows.map(row => ({
      id: row.id as string,
      entityId: row.entity_id as string,
      changeType: row.change_type as SemanticChange['changeType'],
      entityType: row.entity_type as string,
      entityName: row.entity_name as string,
      filePath: row.file_path as string,
      oldFilePath: (row.old_file_path as string) || undefined,
      beforeContent: (row.before_content as string) || undefined,
      afterContent: (row.after_content as string) || undefined,
      commitSha: (row.commit_sha as string) || undefined,
      author: (row.author as string) || undefined,
      timestamp: (row.timestamp as string) || undefined,
    }));
  }

  query(sql: string): unknown[] {
    return this.db.prepare(sql).all();
  }

  close(): void {
    this.db.close();
  }
}
