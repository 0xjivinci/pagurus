//! A tiny inverted index over the transcripts, in one compact file
//! (`$PAGURUS_HOME/index/sessions.idx`), brought up to date before each search
//! by reading only what was appended since. Ranked with BM25.
//!
//! Layout (all integers LEB128 varints, strings length-prefixed):
//!   "HMIX1"
//!   sessions: n, then (name, bytes indexed) …
//!   docs:     n, then (session, line offset, time, length in terms) …
//!   terms:    n, then (term, doc count, last doc, posting bytes, postings) …
//!             postings: (doc − previous doc, term frequency) …, sorted by term
//! An update rewrites the file, copying old posting lists byte for byte and
//! appending the new documents to them: memory stays at the size of the update.

use super::{Hit, Query};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

const MAGIC: &[u8] = b"HMIX1";

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn num(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.b.get(self.pos)?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    fn str(&mut self) -> Option<&'a str> {
        let n = self.num()? as usize;
        std::str::from_utf8(self.bytes(n)?).ok()
    }
}

struct Doc {
    session: u32,
    off: u64,
    t: u64,
    len: u32,
}

/// The index minus its term section, which stays as bytes in `raw[terms..]`.
#[derive(Default)]
struct Index {
    sessions: Vec<(String, u64)>,
    docs: Vec<Doc>,
    raw: Vec<u8>,
    terms: usize,
}

fn decode(raw: Vec<u8>) -> Option<Index> {
    let mut r = Reader { b: &raw, pos: 0 };
    if r.bytes(MAGIC.len())? != MAGIC {
        return None;
    }
    let mut sessions = Vec::new();
    for _ in 0..r.num()? {
        sessions.push((r.str()?.to_string(), r.num()?));
    }
    let mut docs = Vec::new();
    for _ in 0..r.num()? {
        docs.push(Doc { session: r.num()? as u32, off: r.num()?, t: r.num()?, len: r.num()? as u32 });
    }
    let terms = r.pos;
    Some(Index { sessions, docs, raw, terms })
}

/// Each term entry of the term section: (term, doc count, last doc, postings).
fn entries<'a>(raw: &'a [u8], mut f: impl FnMut(&'a str, u64, u64, &'a [u8])) -> Option<()> {
    let mut r = Reader { b: raw, pos: 0 };
    for _ in 0..r.num()? {
        let term = r.str()?;
        let (df, last, n) = (r.num()?, r.num()?, r.num()? as usize);
        f(term, df, last, r.bytes(n)?);
    }
    Some(())
}

fn path(home: &Path) -> std::path::PathBuf {
    home.join("index").join("sessions.idx")
}

/// Load the index and add whatever the transcripts gained since; rebuilt from
/// scratch if it is missing, unreadable, or a transcript shrank.
fn refresh(home: &Path) -> Result<Index, String> {
    let files = super::session_files(home);
    let mut ix = std::fs::read(path(home)).ok().and_then(decode).unwrap_or_default();
    let known: HashMap<String, (usize, u64)> =
        ix.sessions.iter().enumerate().map(|(i, (n, b))| (n.clone(), (i, *b))).collect();
    if ix.raw.is_empty() || files.iter().any(|(n, len)| known.get(n).is_some_and(|k| *len < k.1)) {
        ix = Index::default();
    }
    let mut new_terms: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
    let mut changed = ix.raw.is_empty();
    for (name, len) in &files {
        let (si, from) = match known.get(name).filter(|_| !ix.raw.is_empty()) {
            Some(&(i, b)) => (i, b),
            None => {
                ix.sessions.push((name.clone(), 0));
                (ix.sessions.len() - 1, 0)
            }
        };
        if *len == from {
            continue;
        }
        let end = super::scan_from(home, name, from, |off, t, _role, text| {
            let id = ix.docs.len() as u32;
            let mut tf: HashMap<String, u32> = HashMap::new();
            let mut n = 0;
            for (_, term) in super::tokens(text) {
                *tf.entry(term).or_default() += 1;
                n += 1;
            }
            for (term, c) in tf {
                new_terms.entry(term).or_default().push((id, c));
            }
            ix.docs.push(Doc { session: si as u32, off, t, len: n });
        });
        changed |= end != from;
        ix.sessions[si].1 = end;
    }
    if !changed {
        return Ok(ix);
    }
    // write: header, docs, then old and new terms merged in order
    let mut out = MAGIC.to_vec();
    put(&mut out, ix.sessions.len() as u64);
    for (n, b) in &ix.sessions {
        put_str(&mut out, n);
        put(&mut out, *b);
    }
    put(&mut out, ix.docs.len() as u64);
    for d in &ix.docs {
        for v in [u64::from(d.session), d.off, d.t, u64::from(d.len)] {
            put(&mut out, v);
        }
    }
    let mut old: Vec<(&str, u64, u64, &[u8])> = Vec::new();
    if !ix.raw.is_empty() {
        entries(&ix.raw[ix.terms..], |t, df, last, p| old.push((t, df, last, p))).ok_or("corrupt index")?;
    }
    let mut body = Vec::new();
    let mut count = 0u64;
    let mut emit = |term: &str, df: u64, last: u64, postings: &[u8]| {
        put_str(&mut body, term);
        put(&mut body, df);
        put(&mut body, last);
        put(&mut body, postings.len() as u64);
        body.extend_from_slice(postings);
        count += 1;
    };
    let mut fresh = new_terms.iter().peekable();
    for (term, df, last, postings) in old {
        while let Some((t, p)) = fresh.next_if(|(t, _)| t.as_str() < term) {
            let (bytes, last) = encode(p, None);
            emit(t, p.len() as u64, last, &bytes);
        }
        match fresh.next_if(|(t, _)| t.as_str() == term) {
            Some((_, p)) => {
                let (more, new_last) = encode(p, Some(last));
                emit(term, df + p.len() as u64, new_last, &[postings, &more].concat());
            }
            None => emit(term, df, last, postings),
        }
    }
    for (t, p) in fresh {
        let (bytes, last) = encode(p, None);
        emit(t, p.len() as u64, last, &bytes);
    }
    let terms = out.len();
    put(&mut out, count);
    out.extend_from_slice(&body);
    let p = path(home);
    std::fs::create_dir_all(p.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("tmp~");
    std::fs::write(&tmp, &out).and_then(|_| std::fs::rename(&tmp, &p)).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(Index { raw: out, terms, ..ix })
}

/// Postings as delta-encoded bytes, continuing after doc `after`; returns the last doc.
fn encode(p: &[(u32, u32)], after: Option<u64>) -> (Vec<u8>, u64) {
    let mut out = Vec::new();
    let mut prev = after;
    for &(d, tf) in p {
        put(&mut out, u64::from(d) - prev.unwrap_or(0));
        put(&mut out, u64::from(tf));
        prev = Some(u64::from(d));
    }
    (out, prev.unwrap_or(0))
}

pub fn search(home: &Path, q: &Query, terms: &[(String, bool)]) -> Result<Vec<Hit>, String> {
    let ix = refresh(home)?;
    if ix.docs.is_empty() {
        return Ok(Vec::new());
    }
    let alive: std::collections::HashSet<String> = super::session_files(home).into_iter().map(|f| f.0).collect();
    let n = ix.docs.len() as f64;
    let avg = ix.docs.iter().map(|d| f64::from(d.len)).sum::<f64>() / n;
    // doc → (BM25 score, which query terms matched)
    let mut scores: HashMap<u32, (f64, u32)> = HashMap::new();
    entries(&ix.raw[ix.terms..], |term, df, _, postings| {
        let Some(qi) = terms.iter().position(|(t, p)| if *p { term.starts_with(t.as_str()) } else { term == t }) else {
            return;
        };
        let idf = (1.0 + (n - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
        let mut r = Reader { b: postings, pos: 0 };
        let mut doc = 0u64;
        let mut first = true;
        while let (Some(delta), Some(tf)) = (r.num(), r.num()) {
            doc = if first { delta } else { doc + delta };
            first = false;
            let Some(d) = ix.docs.get(doc as usize) else { break };
            let tf = tf as f64;
            let s = idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * f64::from(d.len) / avg));
            let e = scores.entry(doc as u32).or_default();
            e.0 += s;
            e.1 |= 1 << qi;
        }
    })
    .ok_or("corrupt index")?;
    let now = crate::util::now();
    let mut ranked: Vec<(f64, u32)> = scores
        .into_iter()
        .filter_map(|(id, (s, mask))| {
            let d = &ix.docs[id as usize];
            let sid = &ix.sessions.get(d.session as usize)?.0;
            if !alive.contains(sid) || !q.keeps(sid, d.t) {
                return None;
            }
            // reward matching every term; a mild nudge towards recent messages
            let cover = f64::from(mask.count_ones()) / terms.len() as f64;
            let age_days = now.saturating_sub(d.t) as f64 / 86_400.0;
            Some((s * cover * cover * (1.0 + 0.2 * (-age_days / 30.0).exp()), id))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut per_session: HashMap<u32, usize> = HashMap::new();
    let mut hits = Vec::new();
    let msg = |id: u32| -> Option<(String, u64, String, String)> {
        let d = ix.docs.get(id as usize)?;
        let sid = ix.sessions.get(d.session as usize)?.0.clone();
        let (t, role, text) = super::read_at(home, &sid, d.off)?;
        Some((sid, t, role, text))
    };
    let neighbour = |id: Option<u32>, session: u32| {
        let id = id?;
        (ix.docs.get(id as usize)?.session == session).then(|| msg(id)).flatten().map(|m| (m.2, m.3))
    };
    for (_, id) in ranked {
        let session = ix.docs[id as usize].session;
        let seen = per_session.entry(session).or_default();
        if *seen >= 2 {
            continue; // two per conversation, so one long chat doesn't crowd out the rest
        }
        let Some((sid, t, role, text)) = msg(id) else { continue };
        *seen += 1;
        hits.push(Hit {
            sid,
            t,
            role,
            text,
            prev: neighbour(id.checked_sub(1), session),
            next: neighbour(Some(id + 1), session),
        });
        if hits.len() >= q.limit {
            break;
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{append, home_with};

    #[test]
    fn index_builds_updates_incrementally_and_rebuilds() {
        let home = home_with(&[("100-cli-a", &[(100, "user", "alpha beta"), (101, "assistant", "beta gamma")])]);
        let ix = super::refresh(&home).unwrap();
        assert_eq!((ix.sessions.len(), ix.docs.len()), (1, 2));
        let size = std::fs::metadata(super::path(&home)).unwrap().len();
        assert!(size < 200, "compact: {size} bytes");
        // nothing new: the file is left alone
        let before = std::fs::metadata(super::path(&home)).unwrap().modified().unwrap();
        super::refresh(&home).unwrap();
        assert_eq!(std::fs::metadata(super::path(&home)).unwrap().modified().unwrap(), before);
        // appended lines and a new session merge into the old posting lists
        append(&home, "100-cli-a", &[(102, "user", "beta delta")]);
        append(&home, "200-tg-b", &[(200, "user", "alpha again")]);
        let ix = super::refresh(&home).unwrap();
        assert_eq!(ix.docs.len(), 4);
        let mut beta = (0, 0, Vec::new());
        super::entries(&ix.raw[ix.terms..], |t, df, last, p| {
            if t == "beta" {
                beta = (df, last, p.to_vec());
            }
        })
        .unwrap();
        assert_eq!((beta.0, beta.1), (3, 2), "beta in docs 0, 1, 2");
        assert_eq!(beta.2, vec![0, 1, 1, 1, 1, 1], "deltas and term frequencies");
        // the same as a build from scratch
        std::fs::remove_file(super::path(&home)).unwrap();
        let fresh = super::refresh(&home).unwrap();
        assert_eq!(fresh.raw, ix.raw);
        // a garbled file or a shrunken transcript triggers a rebuild
        std::fs::write(super::path(&home), b"HMIX1\xff\xff").unwrap();
        assert_eq!(super::refresh(&home).unwrap().docs.len(), 4);
        std::fs::write(home.join("sessions/100-cli-a.jsonl"), "").unwrap();
        assert_eq!(super::refresh(&home).unwrap().docs.len(), 1);
    }

    #[test]
    fn bm25_prefers_rare_terms_and_full_matches() {
        let home = home_with(&[(
            "100-cli-a",
            &[
                (100, "user", "pihole pihole dns"),
                (101, "user", "dns dns dns"),
                (102, "user", "dns resolver for the pihole is unbound"),
                (103, "user", "something else"),
            ],
        )]);
        let q = super::super::Query::new("unbound dns", 3);
        let hits = super::search(&home, &q, &super::super::query_terms(&q.text)).unwrap();
        assert!(hits[0].text.contains("unbound"), "both terms, one rare: {:?}", hits[0].text);
        assert_eq!(hits.len(), 2, "two per session");
        assert_eq!(hits[0].prev.as_ref().unwrap().1, "dns dns dns");
        assert_eq!(hits[0].next.as_ref().unwrap().1, "something else");
    }
}
