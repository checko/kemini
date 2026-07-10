//! Builtin memory backend: SQLite index over `MEMORY.md` + `memory/*.md`,
//! schema-compatible with the npm `memory-core` plugin
//! (`~/.openclaw/memory/<agentId>.sqlite`: meta/files/chunks/embedding_cache/chunks_fts).
//!
//! With `agents.defaults.memorySearch.provider: "none"` search is keyword-only
//! (FTS5), which matches the live installation this port targets.

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct MemoryIndex {
    conn: Connection,
    workspace: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
    pub score: f64,
}

impl MemoryIndex {
    pub fn open(db_path: &Path, workspace: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let idx = Self { conn, workspace: workspace.to_path_buf() };
        idx.ensure_schema()?;
        Ok(idx)
    }

    fn ensure_schema(&self) -> Result<()> {
        // DDL mirrors the npm memory-core schema (verified live).
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS files (
      path TEXT PRIMARY KEY,
      source TEXT NOT NULL DEFAULT 'memory',
      hash TEXT NOT NULL,
      mtime INTEGER NOT NULL,
      size INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS chunks (
      id TEXT PRIMARY KEY,
      path TEXT NOT NULL,
      source TEXT NOT NULL DEFAULT 'memory',
      start_line INTEGER NOT NULL,
      end_line INTEGER NOT NULL,
      hash TEXT NOT NULL,
      model TEXT NOT NULL,
      text TEXT NOT NULL,
      embedding TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
    CREATE INDEX IF NOT EXISTS idx_chunks_source ON chunks(source);
    CREATE TABLE IF NOT EXISTS embedding_cache (
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        provider_key TEXT NOT NULL,
        hash TEXT NOT NULL,
        embedding TEXT NOT NULL,
        dims INTEGER,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (provider, model, provider_key, hash)
    );
    CREATE INDEX IF NOT EXISTS idx_embedding_cache_updated_at ON embedding_cache(updated_at);
    CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text,
  id UNINDEXED,
  path UNINDEXED,
  source UNINDEXED,
  model UNINDEXED,
  start_line UNINDEXED,
  end_line UNINDEXED
);",
        )?;
        Ok(())
    }

    /// Discover memory files: `MEMORY.md` plus `memory/**` recursive `.md`
    /// (skipping symlinks), matching listMemoryFiles in the npm impl.
    pub fn memory_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let top = self.workspace.join("MEMORY.md");
        if top.exists() {
            out.push(top);
        }
        let mut stack = vec![self.workspace.join("memory")];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
                    if meta.is_symlink() {
                        continue;
                    }
                    if meta.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|x| x == "md") {
                        files.push(p);
                    }
                }
            }
        }
        files.sort();
        out.extend(files);
        out
    }

    /// Re-index changed files (hash+mtime comparison), remove rows for deleted files.
    pub fn sync(&mut self) -> Result<usize> {
        let files = self.memory_files();
        let mut updated = 0usize;

        let known: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT path FROM files WHERE source='memory'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };
        let live: std::collections::HashSet<String> = files
            .iter()
            .map(|p| self.rel_path(p))
            .collect();
        for stale in known.iter().filter(|k| !live.contains(*k)) {
            self.remove_file(stale)?;
        }

        for file in &files {
            let rel = self.rel_path(file);
            let text = std::fs::read_to_string(file).unwrap_or_default();
            let hash = hex::encode(Sha256::digest(text.as_bytes()));
            let meta = std::fs::metadata(file)?;
            let mtime = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let existing: Option<String> = self
                .conn
                .query_row("SELECT hash FROM files WHERE path=?1", [&rel], |r| r.get(0))
                .ok();
            if existing.as_deref() == Some(hash.as_str()) {
                continue;
            }
            self.remove_file(&rel)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO files (path, source, hash, mtime, size) VALUES (?1,'memory',?2,?3,?4)",
                rusqlite::params![rel, hash, mtime, text.len() as i64],
            )?;
            // npm chunking: tokens=400 → maxChars 1600, overlap=80 → 320 chars.
            for (start, end, chunk) in chunk_markdown(&text, 1600, 320) {
                let chunk_hash = hex::encode(Sha256::digest(chunk.as_bytes()));
                // Chunk id derivation mirrors the npm impl:
                // sha256("{source}:{path}:{startLine}:{endLine}:{chunkHash}:{model}")
                let id = hex::encode(Sha256::digest(
                    format!("memory:{rel}:{start}:{end}:{chunk_hash}:fts-only").as_bytes(),
                ));
                self.conn.execute(
                    "INSERT OR REPLACE INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding)
                     VALUES (?1,?2,'memory',?3,?4,?5,'fts-only',?6,'[]')",
                    rusqlite::params![id, rel, start, end, chunk_hash, chunk],
                )?;
                self.conn.execute(
                    "INSERT INTO chunks_fts (text, id, path, source, model, start_line, end_line)
                     VALUES (?1,?2,?3,'memory','fts-only',?4,?5)",
                    rusqlite::params![chunk, id, rel, start, end],
                )?;
            }
            updated += 1;
        }
        if updated > 0 {
            // Meta blob matches the npm fts-only mode identity.
            self.conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('memory_index_meta_v1', ?1)",
                [r#"{"model":"fts-only","provider":"none","chunkTokens":400,"chunkOverlap":80}"#],
            )?;
        }
        Ok(updated)
    }

    fn remove_file(&self, rel: &str) -> Result<()> {
        self.conn.execute("DELETE FROM files WHERE path=?1", [rel])?;
        self.conn.execute(
            "DELETE FROM chunks_fts WHERE id IN (SELECT id FROM chunks WHERE path=?1)",
            [rel],
        )?;
        self.conn.execute("DELETE FROM chunks WHERE path=?1", [rel])?;
        Ok(())
    }

    fn rel_path(&self, p: &Path) -> String {
        p.strip_prefix(&self.workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string()
    }

    /// Keyword (FTS5) search, npm-parity for `memorySearch.provider: "none"`:
    /// tokens extracted as unicode word chars, quoted, joined with AND;
    /// bm25 rank mapped to a 0..1 score. Falls back to OR when AND finds
    /// nothing (relaxed fallback).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let tokens: Vec<String> = regex::Regex::new(r"[\p{L}\p{N}_]+")
            .unwrap()
            .find_iter(query)
            .map(|m| format!("\"{}\"", m.as_str().replace('"', "")))
            .collect();
        if tokens.is_empty() {
            return Ok(vec![]);
        }
        let strict = tokens.join(" AND ");
        let hits = self.fts(&strict, limit)?;
        if !hits.is_empty() || tokens.len() < 2 {
            return Ok(hits);
        }
        self.fts(&tokens.join(" OR "), limit)
    }

    fn fts(&self, fts_query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, start_line, end_line, text, bm25(chunks_fts) AS rank
             FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |r| {
            let rank: f64 = r.get::<_, f64>(4).unwrap_or(0.0);
            // bm25RankToScore: rank<0 → r/(1+r) with r=-rank, else 1/(1+rank)
            let score = if rank < 0.0 {
                let r = -rank;
                r / (1.0 + r)
            } else {
                1.0 / (1.0 + rank)
            };
            Ok(SearchHit {
                path: r.get(0)?,
                start_line: r.get(1)?,
                end_line: r.get(2)?,
                text: r.get(3)?,
                score,
            })
        })?;
        Ok(rows.flatten().collect())
    }

    /// Read a memory file with npm-parity bounds: default 120 lines, 12000
    /// char budget fitted on line boundaries, continuation marker with
    /// nextFrom. Returns (text, truncated, next_from).
    pub fn get(
        &self,
        rel_path: &str,
        from: Option<usize>,
        lines: Option<usize>,
    ) -> Result<(String, bool, Option<usize>)> {
        const MAX_CHARS: usize = 12_000;
        let path = self.workspace.join(rel_path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let all: Vec<&str> = text.lines().collect();
        let start = from.unwrap_or(1).max(1) - 1;
        let start = start.min(all.len());
        let want = lines.unwrap_or(120).max(1);
        let hard_end = (start + want).min(all.len());

        let mut out = String::new();
        let mut end = start;
        for (i, line) in all[start..hard_end].iter().enumerate() {
            if !out.is_empty() && out.len() + line.len() + 1 > MAX_CHARS {
                break;
            }
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line);
            end = start + i + 1;
        }
        let truncated = end < all.len();
        let next_from = truncated.then_some(end + 1);
        if truncated {
            out.push_str(&format!(
                "\n[More content available. Use from={} to continue.]",
                end + 1
            ));
        }
        Ok((out, truncated, next_from))
    }
}

/// Char-budget line-oriented chunking mirroring chunkMarkdown in the npm
/// impl: accumulate lines until `max_chars` exceeded, flush, carry a trailing
/// `overlap_chars` window. Returns (start_line, end_line, text) 1-based.
fn chunk_markdown(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<(i64, i64, String)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return vec![];
    }
    let mut out: Vec<(i64, i64, String)> = Vec::new();
    let mut buf: Vec<(usize, &str)> = Vec::new(); // (line_no_1based, text)
    let mut buf_chars = 0usize;

    let flush = |buf: &mut Vec<(usize, &str)>, buf_chars: &mut usize, out: &mut Vec<(i64, i64, String)>| {
        if buf.is_empty() {
            return;
        }
        let start = buf.first().unwrap().0 as i64;
        let end = buf.last().unwrap().0 as i64;
        let text: String = buf.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
        out.push((start, end, text));
        // carry overlap: keep trailing lines up to overlap_chars
        let mut kept: Vec<(usize, &str)> = Vec::new();
        let mut kept_chars = 0usize;
        for &(n, l) in buf.iter().rev() {
            if kept_chars + l.len() > overlap_chars && !kept.is_empty() {
                break;
            }
            kept_chars += l.len() + 1;
            kept.push((n, l));
            if kept_chars >= overlap_chars {
                break;
            }
        }
        kept.reverse();
        *buf = kept;
        *buf_chars = kept_chars;
    };

    for (i, line) in lines.iter().enumerate() {
        if buf_chars + line.len() > max_chars && !buf.is_empty() {
            flush(&mut buf, &mut buf_chars, &mut out);
        }
        buf.push((i + 1, line));
        buf_chars += line.len() + 1;
    }
    if !buf.is_empty() {
        let start = buf.first().unwrap().0 as i64;
        let end = buf.last().unwrap().0 as i64;
        // Skip emitting a final chunk that is pure overlap of the previous one.
        let is_pure_overlap = out
            .last()
            .is_some_and(|(_, prev_end, _)| *prev_end >= end);
        if !is_pure_overlap {
            let text: String = buf.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
            out.push((start, end, text));
        }
    }
    out
}
