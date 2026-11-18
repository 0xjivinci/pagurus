//! The `sqlite` feature: session_search on SQLite FTS5 (`$PAGURUS_HOME/index/sessions.db`),
//! fed from the JSONL transcripts, which stay the source of truth. Existing
//! transcripts are indexed on the first search, appended lines on later ones.

use super::{Hit, Query};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files(name TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(content, sid UNINDEXED, t UNINDEXED, role UNINDEXED,
  tokenize = 'porter unicode61');
";

fn open(home: &Path) -> rusqlite::Result<Connection> {
    let dir = home.join("index");
    let _ = std::fs::create_dir_all(&dir);
    let db = Connection::open(dir.join("sessions.db"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch(SCHEMA)?;
    Ok(db)
}

/// Index what the transcripts gained since the last search.
fn refresh(home: &Path, db: &mut Connection) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    for (name, len) in super::session_files(home) {
        let mut from: u64 =
            tx.query_row("SELECT bytes FROM files WHERE name = ?1", [&name], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
        if len == from {
            continue;
        }
        if len < from {
            // rewritten: start over for this file
            tx.execute("DELETE FROM msgs WHERE sid = ?1", [&name])?;
            from = 0;
        }
        let mut err = None;
        let end = {
            let mut ins = tx.prepare_cached("INSERT INTO msgs(content, sid, t, role) VALUES (?1, ?2, ?3, ?4)")?;
            super::scan_from(home, &name, from, |_, t, role, text| {
                if err.is_none() {
                    err = ins.execute(params![text, name, t as i64, role]).err();
                }
            })
        };
        if let Some(e) = err {
            return Err(e);
        }
        tx.execute("INSERT OR REPLACE INTO files(name, bytes) VALUES (?1, ?2)", params![name, end as i64])?;
    }
    tx.commit()
}

/// An FTS5 query from the terms: each quoted (no operator injection), `*` for prefixes.
fn expr(terms: &[(String, bool)], op: &str) -> String {
    let q: Vec<String> = terms.iter().map(|(t, p)| format!("\"{}\"{}", t.replace('"', ""), if *p { "*" } else { "" })).collect();
    q.join(op)
}

pub fn search(home: &Path, q: &Query, terms: &[(String, bool)]) -> Result<Vec<Hit>, String> {
    let run = || -> rusqlite::Result<Vec<Hit>> {
        let mut db = open(home)?;
        refresh(home, &mut db)?;
        let mut hits: Vec<Hit> = Vec::new();
        let mut seen: Vec<i64> = Vec::new();
        let mut per_session: std::collections::HashMap<String, usize> = Default::default();
        let mut stmt = db.prepare(
            "SELECT rowid, sid, t, role, content FROM msgs WHERE msgs MATCH ?1 ORDER BY bm25(msgs) LIMIT 400",
        )?;
        let mut around =
            db.prepare("SELECT role, content FROM msgs WHERE rowid = ?1 AND sid = ?2")?;
        // every term first, then any of them to fill up
        for op in [" ", " OR "] {
            let rows = stmt.query_map([expr(terms, op)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
            })?;
            for row in rows {
                let (rowid, sid, t, role, text) = row?;
                if hits.len() >= q.limit {
                    break;
                }
                let n = per_session.entry(sid.clone()).or_default();
                if seen.contains(&rowid) || *n >= 2 || !q.keeps(&sid, t as u64) {
                    continue;
                }
                *n += 1;
                seen.push(rowid);
                let mut near = |id: i64| around.query_row(params![id, sid], |r| Ok((r.get(0)?, r.get(1)?))).optional();
                let (prev, next) = (near(rowid - 1)?, near(rowid + 1)?);
                hits.push(Hit { sid, t: t as u64, role, text, prev, next });
            }
            if hits.len() >= q.limit || terms.len() == 1 {
                break;
            }
        }
        Ok(hits)
    };
    run().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{append, home_with};

    fn find(home: &std::path::Path, text: &str) -> Vec<super::Hit> {
        let q = super::super::Query::new(text, 5);
        super::search(home, &q, &super::super::query_words(text, false)).unwrap()
    }

    #[test]
    fn fts5_indexes_jsonl_incrementally_and_ranks() {
        let home = home_with(&[(
            "100-tg-a",
            &[(100, "user", "the cloudflare tunnel url changed"), (101, "assistant", "restarted cloudflared"), (102, "user", "tunnels again?")],
        )]);
        let hits = find(&home, "cloudflare tunnels");
        assert!(hits[0].text.contains("cloudflare tunnel"), "all terms first, porter-stemmed");
        assert_eq!(hits[0].next.as_ref().unwrap().1, "restarted cloudflared");
        assert_eq!(hits.len(), 2, "then any term, two per session");
        append(&home, "200-cli-b", &[(200, "user", "kubernetes running on the pi")]);
        let hits = find(&home, "kube*");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].prev.is_none() && hits[0].sid == "200-cli-b");
        assert_eq!(find(&home, "run").len(), 1, "stemming: run finds running");
        // quotes and operators in the query can't break out of the FTS5 expression
        assert!(find(&home, "\"kubernetes\" OR NEAR(").len() == 1);
        // a rewritten transcript is indexed again, not twice
        std::fs::write(home.join("sessions/200-cli-b.jsonl"), "").unwrap();
        append(&home, "200-cli-b", &[(201, "user", "k3s")]);
        assert!(find(&home, "kubernetes").is_empty());
        let r = super::super::search(&home, &super::super::Query::new("tunnel", 5));
        assert!(r.contains("» user: the cloudflare tunnel url changed"), "{r}");
    }
}
