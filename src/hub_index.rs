//! The skills hub (uge): `pagurus skills search <words>`, `install <name>` and
//! the `skill_hub` tool use the public Hermes skills index (about 100k entries
//! from Hermes' own skills, GitHub repos, skills.sh and more). pagurus keeps the
//! ~20k that live on GitHub as a compact local copy (cache/skills-index.tsv),
//! streamed in so a Pi never holds the 45 MB JSON in memory, and refreshes it
//! weekly. `tools.skills_hub` points elsewhere or switches it off.

use crate::config::Config;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// The public Hermes skills index (Nous Research): JSON with a `skills` array.
pub const HERMES_INDEX: &str = "https://hermes-agent.nousresearch.com/docs/api/skills-index.json";
const INDEX_MAX_AGE_SECS: u64 = 7 * 86400;
const INDEX_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// One installable skill from the index.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub trust: String,
    pub source: String,
    pub name: String,
    pub desc: String,
    /// GitHub owner/repo and the skill's folder in it: together the id
    pub repo: String,
    pub path: String,
    pub tags: String,
}

impl Entry {
    pub fn id(&self) -> String {
        if self.path.is_empty() { self.repo.clone() } else { format!("{}/{}", self.repo, self.path) }
    }

    fn url(&self) -> String {
        format!("https://github.com/{}/tree/HEAD/{}", self.repo, self.path)
    }

    fn line(&self) -> String {
        let f = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\n", f(&self.trust), f(&self.source), f(&self.name), f(&self.desc), f(&self.repo), f(&self.path), f(&self.tags))
    }

    fn parse(l: &str) -> Option<Entry> {
        let mut it = l.split('\t').map(String::from);
        Some(Entry { trust: it.next()?, source: it.next()?, name: it.next()?, desc: it.next()?, repo: it.next()?, path: it.next()?, tags: it.next().unwrap_or_default() })
    }
}

/// One entry as the index has it; only what pagurus needs, the rest is skipped.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Raw {
    name: String,
    description: String,
    source: String,
    trust_level: String,
    repo: String,
    path: String,
    resolved_github_id: Option<String>,
    tags: Vec<String>,
}

impl Raw {
    /// Only skills with a SKILL.md on GitHub can be installed.
    fn entry(self) -> Option<Entry> {
        let (repo, path) = match self.resolved_github_id.as_deref().filter(|r| r.matches('/').count() >= 1) {
            Some(r) => {
                let mut p = r.splitn(3, '/');
                let (o, n) = (p.next()?, p.next()?);
                (format!("{o}/{n}"), p.next().unwrap_or("").to_string())
            }
            None => (self.repo, self.path),
        };
        if repo.matches('/').count() != 1 || self.name.trim().is_empty() || self.name.len() > 80 {
            return None;
        }
        Some(Entry {
            trust: self.trust_level,
            source: self.source,
            name: self.name.trim().to_string(),
            desc: crate::util::clip(self.description.trim(), 240),
            repo,
            path: path.trim_matches('/').to_string(),
            tags: self.tags.join(","),
        })
    }
}

/// Streams `{"skills": [ ... ]}` into TSV lines, one entry at a time.
struct Skills<'a, W: Write>(&'a mut W, &'a mut usize);

impl<'de, W: Write> serde::de::DeserializeSeed<'de> for Skills<'_, W> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de, W: Write> serde::de::Visitor<'de> for Skills<'_, W> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a list of skills")
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(raw) = seq.next_element::<Raw>()? {
            if let Some(e) = raw.entry() {
                self.0.write_all(e.line().as_bytes()).map_err(serde::de::Error::custom)?;
                *self.1 += 1;
            }
        }
        Ok(())
    }
}

struct Index<'a, W: Write>(&'a mut W, &'a mut usize);

impl<'de, W: Write> serde::de::Visitor<'de> for Index<'_, W> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a skills index object")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(k) = map.next_key::<String>()? {
            if k == "skills" {
                map.next_value_seed(Skills(&mut *self.0, &mut *self.1))?;
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

/// Convert an index (any reader) into the compact TSV. Returns how many entries.
fn convert(r: impl Read, out: &mut impl Write) -> Result<usize, String> {
    let mut n = 0;
    let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::new(r));
    serde::Deserializer::deserialize_map(&mut de, Index(out, &mut n)).map_err(|e| format!("unreadable skills index: {e}"))?;
    Ok(n)
}

fn index_file(home: &Path) -> std::path::PathBuf {
    home.join("cache").join("skills-index.tsv")
}

fn hub_url(cfg: &Config) -> Result<&str, String> {
    match cfg.tools.skills_hub.trim() {
        "" | "off" => Err("the skills hub is off (tools.skills_hub in config.toml)".into()),
        u if u.starts_with("https://") => Ok(u),
        u => Err(format!("tools.skills_hub `{u}`: use an https URL, or \"off\"")),
    }
}

/// Download the index again. Returns how many installable skills it lists.
pub fn refresh(home: &Path, cfg: &Config) -> Result<usize, String> {
    let url = hub_url(cfg)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirects(3)
        .user_agent(concat!("pagurus/", env!("CARGO_PKG_VERSION")))
        .build();
    let r = agent.get(url).call().map_err(|e| format!("could not fetch the skills index: {e}"))?;
    let path = index_file(home);
    std::fs::create_dir_all(path.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(|e| e.to_string())?);
    let n = convert(r.into_reader().take(INDEX_MAX_BYTES), &mut out);
    let n = n.and_then(|n| out.flush().map(|_| n).map_err(|e| e.to_string()));
    match n {
        Ok(n) if n > 0 => {
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
            Ok(n)
        }
        other => {
            let _ = std::fs::remove_file(&tmp);
            Err(other.err().unwrap_or_else(|| "the skills index lists no installable skills".into()))
        }
    }
}

/// The local copy, downloaded or refreshed when missing or over a week old
/// (an old copy is used when the download fails).
fn entries(home: &Path, cfg: &Config) -> Result<Vec<Entry>, String> {
    let path = index_file(home);
    let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs());
    if age.is_none_or(|a| a > INDEX_MAX_AGE_SECS) {
        if let Err(e) = refresh(home, cfg) {
            if age.is_none() {
                return Err(e);
            }
        }
    }
    Ok(crate::util::read(&path).lines().filter_map(Entry::parse).collect())
}

/// Best matches for `query`, most relevant first: every word must appear in the
/// name, tags or description; name hits count most, reviewed sources rank higher.
pub fn search(home: &Path, cfg: &Config, query: &str, n: usize) -> Result<Vec<Entry>, String> {
    Ok(rank(entries(home, cfg)?, query, n))
}

fn rank(all: Vec<Entry>, query: &str, n: usize) -> Vec<Entry> {
    let words: Vec<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 2).map(String::from).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(i64, Entry)> = all
        .into_iter()
        .filter_map(|e| {
            let (name, tags, desc) = (e.name.to_lowercase(), e.tags.to_lowercase(), e.desc.to_lowercase());
            let mut score = 0i64;
            for w in &words {
                let s = if name.contains(w.as_str()) { 6 } else if tags.contains(w.as_str()) { 3 } else if desc.contains(w.as_str()) { 1 } else { return None };
                score += s;
            }
            score += match e.trust.as_str() {
                "builtin" => 4,
                "trusted" => 2,
                _ => 0,
            };
            Some((score, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
    let mut seen = std::collections::HashSet::new();
    scored.into_iter().map(|(_, e)| e).filter(|e| seen.insert(e.id())).take(n).collect()
}

/// Search results as text (CLI and the agent's skill_hub tool).
pub fn search_text(home: &Path, cfg: &Config, query: &str, n: usize) -> Result<String, String> {
    let hits = search(home, cfg, query, n)?;
    if hits.is_empty() {
        return Ok(format!("no skills in the hub match \"{query}\""));
    }
    let mut out = format!("Skills hub, best matches for \"{query}\" (install with the id):\n");
    for e in hits {
        let trust = if e.trust == "community" { "community ⚠" } else { e.trust.as_str() };
        out += &format!("- {} — {} [{trust}, {}]\n  id: {}\n", e.name, crate::util::clip(&e.desc, 160), e.source, e.id());
    }
    Ok(out)
}

/// A hub name or id → (GitHub URL, trust). None when `src` is a URL or a path.
pub(crate) fn resolve(home: &Path, cfg: &Config, src: &str) -> Result<Option<(String, String)>, String> {
    let s = src.trim();
    if s.starts_with("https://") || s.starts_with("http://") || crate::util::expand(s).exists() || s.starts_with('/') || s.starts_with('.') || s.starts_with('~') {
        return Ok(None);
    }
    let all = entries(home, cfg).map_err(|e| format!("`{s}` is not a URL or a path, and the skills hub can't be searched: {e}"))?;
    if let Some(e) = all.iter().find(|e| e.id() == s) {
        return Ok(Some((e.url(), e.trust.clone())));
    }
    let mut same: Vec<&Entry> = all.iter().filter(|e| e.name.eq_ignore_ascii_case(s)).collect();
    same.sort_by_key(|e| match e.trust.as_str() {
        "builtin" => 0,
        "trusted" => 1,
        _ => 2,
    });
    match same.as_slice() {
        [] => Err(format!("no skill named `{s}` in the hub; try `pagurus skills search {s}`")),
        [e] => Ok(Some((e.url(), e.trust.clone()))),
        // one reviewed source beats any number of community copies
        [e, rest @ ..] if e.trust != "community" && rest.iter().all(|r| r.trust == "community") => Ok(Some((e.url(), e.trust.clone()))),
        many => Err(format!(
            "{} skills are called `{s}`; install one by its id:\n{}",
            many.len(),
            many.iter().take(8).map(|e| format!("  {} [{}] {}", e.id(), e.trust, crate::util::clip(&e.desc, 80))).collect::<Vec<_>>().join("\n")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_streams_into_installable_entries_and_ranks_them() {
        let json = r#"{"version":1,"skills":[
            {"name":"systematic-debugging","description":"Find the root cause before fixing.","source":"official","trust_level":"builtin",
             "repo":"NousResearch/hermes-agent","path":"skills/software-development/systematic-debugging","tags":["debugging"],"extra":{"x":[1,2]}},
            {"name":"debug-helper","description":"Debugging tips\tand more","source":"skills.sh","trust_level":"community",
             "repo":"a/b","path":"debug-helper","resolved_github_id":"a/b/skills/debug-helper","tags":[]},
            {"name":"persona","description":"a chat persona","source":"lobehub","trust_level":"community","repo":"","path":""}
        ],"generated_at":"now"}"#;
        let mut out = Vec::new();
        assert_eq!(convert(json.as_bytes(), &mut out).unwrap(), 2, "the LobeHub persona has no SKILL.md on GitHub");
        let all: Vec<Entry> = String::from_utf8(out).unwrap().lines().filter_map(Entry::parse).collect();
        assert_eq!(all[1].id(), "a/b/skills/debug-helper", "skills.sh: the resolved GitHub path");
        assert_eq!(all[1].desc, "Debugging tips and more", "tabs can't break the file");
        let hits = rank(all.clone(), "debugging", 5);
        assert_eq!(hits[0].name, "systematic-debugging", "name hit + builtin first");
        assert_eq!(rank(all.clone(), "debugging root cause", 5).len(), 1, "every word must match");
        assert!(rank(all, "x", 5).is_empty());
    }

    #[test]
    fn hub_names_resolve_to_github_and_prefer_reviewed_sources() {
        let home = std::env::temp_dir().join(format!("pagurus-hubidx-{}", crate::util::new_id()));
        let e = |trust: &str, repo: &str, name: &str| Entry { trust: trust.into(), source: "s".into(), name: name.into(), desc: "d".into(), repo: repo.into(), path: format!("skills/{name}"), tags: String::new() };
        let lines: String = [e("builtin", "o/official", "pdf"), e("community", "x/copy", "pdf"), e("community", "y/one", "notes"), e("community", "z/two", "notes")].iter().map(Entry::line).collect();
        crate::util::write_atomic(&index_file(&home), &lines).unwrap();
        let cfg = Config::default();
        assert_eq!(resolve(&home, &cfg, "pdf").unwrap().unwrap(), ("https://github.com/o/official/tree/HEAD/skills/pdf".into(), "builtin".into()));
        assert!(resolve(&home, &cfg, "notes").unwrap_err().contains("install one by its id"));
        assert_eq!(resolve(&home, &cfg, "z/two/skills/notes").unwrap().unwrap().0, "https://github.com/z/two/tree/HEAD/skills/notes");
        assert!(resolve(&home, &cfg, "nope").unwrap_err().contains("skills search"));
        assert_eq!(resolve(&home, &cfg, "https://example.com/SKILL.md").unwrap(), None);
        assert_eq!(crate::hub::raw_url("https://github.com/o/official/tree/HEAD/skills/pdf").unwrap().0, "https://raw.githubusercontent.com/o/official/HEAD/skills/pdf/SKILL.md");
        let _ = std::fs::remove_dir_all(&home);
    }
}
