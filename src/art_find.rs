//! Discovery for `art` on Tezos: find artists beyond the top sellers. Quiet
//! artists who sold well before, newcomers, a style (tag), artists collected by
//! the same people, collections by contract, and fuzzy name search. Everything
//! ranks by *different collectors*, the measure that's hardest to fake.

use crate::art::{flag_of, gql, gql_pages, holder_label, num, profile_url, sale, tez, BURN, NOTE, SALE_FIELDS, SALE_FILTER};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

// ---------- time ----------

/// Days since an objkt timestamp ("2026-11-19T03:31:31+00:00").
pub fn days_ago(ts: &Value) -> Option<u64> {
    let s = ts.as_str()?;
    let (y, m, d) = (s.get(0..4)?.parse().ok()?, s.get(5..7)?.parse().ok()?, s.get(8..10)?.parse().ok()?);
    let then = crate::util::days_from_civil(y, m, d);
    let today = (crate::util::now() / 86_400) as i64;
    Some(today.saturating_sub(then).max(0) as u64)
}

/// "2026-11-09, 10 days ago"
pub fn when(ts: &Value) -> String {
    let day: String = ts.as_str().unwrap_or("").chars().take(10).collect();
    match days_ago(ts) {
        Some(0) => format!("{day}, today"),
        Some(1) => format!("{day}, yesterday"),
        Some(n) => format!("{day}, {n} days ago"),
        None => day,
    }
}

fn iso_days_ago(days: u64) -> String {
    crate::util::fmt_utc_iso(crate::util::now().saturating_sub(days * 86_400))
}

// ---------- fuzzy search ----------

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Candidate profiles for a name: username, .tez name or X handle, each part of
/// "a/b" or "a, b" tried. Accounts that minted something come first, and an
/// exact (punctuation-blind) name match first of those.
pub fn search(who: &str) -> Result<Vec<Value>, String> {
    let q = "query($p: String!) { holder(where: {_or: [{alias: {_ilike: $p}}, {tzdomain: {_ilike: $p}}, {twitter: {_ilike: $p}}]}, \
        limit: 8) { address alias tzdomain twitter flag flag_reason created_tokens(limit: 1) { token_pk } } }";
    let mut seen = HashSet::new();
    let mut found: Vec<(i32, Value)> = Vec::new();
    let parts: Vec<String> = who
        .split(['/', ',', '&', '+'])
        .flat_map(|p| p.split(" and "))
        .map(|p| p.trim().trim_start_matches('@').to_string())
        .filter(|p| norm(p).len() >= 3)
        .collect();
    for p in &parts {
        let pattern = format!("%{}%", p.replace(['%', '_'], "").split_whitespace().collect::<Vec<_>>().join("%"));
        for h in gql(q, json!({ "p": pattern }))?["holder"].as_array().cloned().unwrap_or_default() {
            let addr = h["address"].as_str().unwrap_or("").to_string();
            if !seen.insert(addr) {
                continue;
            }
            let minted = h["created_tokens"].as_array().is_some_and(|a| !a.is_empty());
            let names = [h["alias"].as_str(), h["tzdomain"].as_str().map(|d| d.trim_end_matches(".tez"))];
            let exact = names.iter().flatten().any(|n| norm(n) == norm(p));
            let score = i32::from(minted) * 2 + i32::from(exact) * 3;
            found.push((score, h));
        }
    }
    found.sort_by_key(|x| std::cmp::Reverse(x.0));
    Ok(found.into_iter().map(|(_, h)| h).collect())
}

/// The one profile a fuzzy search clearly points to, or the candidates to choose from.
pub fn best_match(who: &str) -> Result<Value, String> {
    let c = search(who)?;
    let artists: Vec<&Value> = c.iter().filter(|h| h["created_tokens"].as_array().is_some_and(|a| !a.is_empty())).collect();
    match (artists.len(), c.len()) {
        (_, 0) => Err(format!("no objkt profile matches \"{who}\"; try their tz… address, .tez name or X handle")),
        (1, _) => Ok(artists[0].clone()),
        (0, 1) => Ok(c[0].clone()),
        _ => Err(format!(
            "several profiles match \"{who}\" (artists first): {}. Ask which one, or use the address.",
            c.iter().take(6).map(candidate).collect::<Vec<_>>().join("; ")
        )),
    }
}

fn candidate(h: &Value) -> String {
    let a = h["address"].as_str().unwrap_or("");
    let mut s = holder_label(h, a);
    if let Some(x) = h["twitter"].as_str().filter(|t| !t.is_empty()) {
        s += &format!(" · X {}", x.rsplit('/').next().unwrap_or(x));
    }
    if h["created_tokens"].as_array().is_some_and(|t| !t.is_empty()) {
        s += " · artist";
    }
    s + &format!(" · {a}")
}

pub fn search_report(who: &str) -> Result<String, String> {
    let c = search(who)?;
    let mut out = format!("{NOTE}\nProfiles matching \"{who}\" ({} found, artists first):\n", c.len());
    for h in c.iter().take(10) {
        out += &format!("- {} · {}\n", candidate(h), profile_url(h["address"].as_str().unwrap_or("")));
    }
    if c.is_empty() {
        out += "- none; try another spelling, their .tez name or X handle\n";
    }
    Ok(out)
}

// ---------- shared: sales samples per artist ----------

struct Stat {
    label: String,
    buyers: HashSet<String>,
    sales: usize,
    volume: i64,
    last: Value,
}

const CREATOR_FIELDS: &str = "token { creators { creator_address holder { alias tzdomain flag flag_reason } } }";

/// Sales between two moments (newest first, up to `limit`), with each token's creator.
fn sales_between(from: &str, to: &str, limit: usize, extra: &str) -> Result<Vec<(Value, Value)>, String> {
    let q = format!(
        "query($from: timestamptz!, $to: timestamptz!, $n: Int!) {{ event(where: {{timestamp: {{_gte: $from, _lt: $to}}, \
         {SALE_FILTER}{extra}}}, order_by: {{timestamp: desc}}, limit: $n) {{ {SALE_FIELDS} {CREATOR_FIELDS} }} }}"
    );
    let rows = gql(&q, json!({ "from": from, "to": to, "n": limit.min(500) }))?["event"].as_array().cloned().unwrap_or_default();
    Ok(rows.into_iter().map(|e| (sale(&e), e["token"]["creators"].get(0).cloned().unwrap_or(Value::Null))).collect())
}

/// Per artist: different buyers, sales, volume and latest sale. Flagged accounts
/// and the burn address are left out, and so is an artist buying their own work.
fn tally(rows: &[(Value, Value)]) -> HashMap<String, Stat> {
    let mut by: HashMap<String, Stat> = HashMap::new();
    for (s, cr) in rows {
        let Some(a) = cr["creator_address"].as_str() else { continue };
        // contracts (KT1…) mint for platforms, not people
        if a == BURN || a.starts_with("KT1") || !flag_of(&cr["holder"]).is_empty() {
            continue;
        }
        let e = by.entry(a.to_string()).or_insert_with(|| Stat {
            label: holder_label(&cr["holder"], a),
            buyers: HashSet::new(),
            sales: 0,
            volume: 0,
            last: Value::Null,
        });
        if let Some(b) = s["buyer_address"].as_str().filter(|b| *b != a) {
            e.buyers.insert(b.to_string());
        }
        e.sales += 1;
        e.volume += num(&s["price_xtz"]).unwrap_or(0);
        if e.last.is_null() || s["timestamp"].as_str() > e.last.as_str() {
            e.last = s["timestamp"].clone();
        }
    }
    by
}

fn ranked(by: HashMap<String, Stat>, min_buyers: usize) -> Vec<(String, Stat)> {
    let mut v: Vec<(String, Stat)> = by.into_iter().filter(|(_, s)| s.buyers.len() >= min_buyers).collect();
    v.sort_by(|a, b| b.1.buyers.len().cmp(&a.1.buyers.len()).then(b.1.volume.cmp(&a.1.volume)));
    v
}

/// Of these artists, which sold or minted since `from`?
fn active_since(addrs: &[String], from: &str) -> Result<(HashSet<String>, HashSet<String>), String> {
    let sold_q = format!(
        "query($a: [String!]!, $from: timestamptz!) {{ event(where: {{token: {{creators: {{creator_address: {{_in: $a}}}}}}, \
         timestamp: {{_gte: $from}}, {SALE_FILTER}}}, limit: 500) {{ token {{ creators {{ creator_address }} }} }} }}"
    );
    let minted_q = "query($a: [String!]!, $from: timestamptz!) { token(where: {creators: {creator_address: {_in: $a}}, \
        timestamp: {_gte: $from}}, limit: 500) { creators { creator_address } } }";
    let creators = |rows: &Value, path: &dyn Fn(&Value) -> Value| -> HashSet<String> {
        rows.as_array()
            .map(|a| a.iter().flat_map(|r| path(r)["creators"].as_array().cloned().unwrap_or_default()))
            .into_iter()
            .flatten()
            .filter_map(|c| c["creator_address"].as_str().map(String::from))
            .filter(|c| addrs.contains(c))
            .collect()
    };
    let sold = creators(&gql(&sold_q, json!({ "a": addrs, "from": from }))?["event"], &|r| r["token"].clone());
    let minted = creators(&gql(minted_q, json!({ "a": addrs, "from": from }))?["token"], &|r| r.clone());
    Ok((sold, minted))
}

// ---------- discovery ----------

/// Artists who sold to several different collectors in the past year but have
/// had no sales in the last `quiet_days`. The sample is spread over the year
/// (six slices), so it isn't only the last few busy days.
pub fn quiet(quiet_days: u64, n: usize) -> Result<String, String> {
    let quiet_days = quiet_days.clamp(14, 300);
    let lookback = 365u64;
    let slices = 6u64;
    let span = (lookback - quiet_days) / slices;
    let mut rows = Vec::new();
    for k in 0..slices {
        let to = iso_days_ago(quiet_days + k * span);
        let from = iso_days_ago(quiet_days + (k + 1) * span);
        rows.extend(sales_between(&from, &to, 400, "")?);
    }
    let sample = rows.len();
    let cands: Vec<(String, Stat)> = ranked(tally(&rows), 3).into_iter().take(60).collect();
    let addrs: Vec<String> = cands.iter().map(|(a, _)| a.clone()).collect();
    let boundary = iso_days_ago(quiet_days);
    let (sold, minted) = if addrs.is_empty() { Default::default() } else { active_since(&addrs, &boundary)? };
    let quiet: Vec<&(String, Stat)> = cands.iter().filter(|(a, _)| !sold.contains(a)).collect();
    let mut out = format!(
        "{NOTE}\nQuiet artists: sold to 3+ different collectors in the past year, but no sales in the last {quiet_days} days \
         (from {sample} sales sampled across the year):\n"
    );
    for (a, st) in quiet.iter().take(n.clamp(1, 25)) {
        out += &format!(
            "- {} · {} collectors in the sample · last sale {} · {} · {}\n",
            st.label,
            st.buyers.len(),
            crate::art_find::when(&st.last),
            if minted.contains(a) { "still minting new work" } else { "no new work since" },
            profile_url(a)
        );
    }
    if quiet.is_empty() {
        out += "- none found in this sample; try a shorter quiet period\n";
    }
    out += "A quiet market can mean a pause, a move to another platform, or an overlooked artist: look one up before deciding.\n";
    Ok(out)
}

/// Artists whose first mint on Tezos is recent and who already sold to 2+ collectors.
pub fn newcomers(started_days: u64, n: usize) -> Result<String, String> {
    let started_days = started_days.clamp(7, 365);
    let now = crate::util::fmt_utc_iso(crate::util::now());
    let mut rows = sales_between(&iso_days_ago(started_days.min(60)), &now, 500, "")?;
    rows.extend(sales_between(&iso_days_ago(started_days.min(60)), &iso_days_ago(3), 500, "")?);
    let cands: Vec<(String, Stat)> = ranked(tally(&rows), 2).into_iter().take(80).collect();
    let addrs: Vec<String> = cands.iter().map(|(a, _)| a.clone()).collect();
    let q = "query($a: [String!]!) { holder(where: {address: {_in: $a}}) { address \
        created_tokens(order_by: {token: {timestamp: asc}}, limit: 1) { token { timestamp } } } }";
    let first: HashMap<String, Value> = if addrs.is_empty() {
        HashMap::new()
    } else {
        gql(q, json!({ "a": addrs }))?["holder"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| Some((h["address"].as_str()?.to_string(), h["created_tokens"][0]["token"]["timestamp"].clone())))
            .collect()
    };
    let fresh: Vec<&(String, Stat)> =
        cands.iter().filter(|(a, _)| first.get(a).and_then(days_ago).is_some_and(|d| d <= started_days)).collect();
    let mut out = format!("{NOTE}\nNewcomers: first mint in the last {started_days} days, already collected by 2+ different people:\n");
    for (a, st) in fresh.iter().take(n.clamp(1, 25)) {
        out += &format!(
            "- {} · first mint {} · {} collectors · {} sales, {} · {}\n",
            st.label,
            when(first.get(a).unwrap_or(&Value::Null)),
            st.buyers.len(),
            st.sales,
            tez(st.volume),
            profile_url(a)
        );
    }
    if fresh.is_empty() {
        out += "- none in the latest sales; try a longer period (e.g. 120 days)\n";
    }
    Ok(out)
}

/// Artists whose work tagged `tag` (a style: generative, pixel, photography…) sold
/// to the most different collectors lately. Ranking by collectors buries tag spam.
pub fn style(tag: &str, days: u64, n: usize) -> Result<String, String> {
    let tag = tag.trim().trim_start_matches('#');
    if tag.len() < 2 {
        return Err("say which style or tag, e.g. generative, pixelart, photography, glitch, landscape, p5js, illustration".into());
    }
    let days = days.clamp(1, 90);
    // 1. artworks with that exact tag minted in the last 90 days (the indexed, fast way to ask)
    let tok_q = "query($tag: String!, $from: timestamptz!) { token(where: {tags: {tag: {name: {_eq: $tag}}}, \
        timestamp: {_gte: $from}}, order_by: {timestamp: desc}, limit: 500) { pk } }";
    let mut pks = Vec::new();
    let mut seen = HashSet::new();
    for t in [tag.to_string(), tag.to_lowercase()] {
        if !seen.insert(t.clone()) {
            continue;
        }
        for x in gql(tok_q, json!({ "tag": t, "from": iso_days_ago(90) }))?["token"].as_array().cloned().unwrap_or_default() {
            if let Some(pk) = x["pk"].as_i64().or_else(|| x["pk"].as_str().and_then(|p| p.parse().ok())) {
                pks.push(pk);
            }
        }
    }
    if pks.is_empty() {
        let mut out = format!("{NOTE}\nNo artworks tagged \"{tag}\" were minted in the last 90 days.\n");
        out += &makers(tag, &HashSet::new(), n.clamp(1, 25))?;
        return Ok(out);
    }
    // 2. their sales in the window
    let q = format!(
        "query($p: [bigint!]!, $from: timestamptz!) {{ event(where: {{token_pk: {{_in: $p}}, timestamp: {{_gte: $from}}, \
         {SALE_FILTER}}}, order_by: {{timestamp: desc}}, limit: 500) {{ {SALE_FIELDS} {CREATOR_FIELDS} }} }}"
    );
    let rows: Vec<(Value, Value)> = gql(&q, json!({ "p": pks, "from": iso_days_ago(days) }))?["event"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|e| (sale(&e), e["token"]["creators"].get(0).cloned().unwrap_or(Value::Null)))
        .collect();
    let list = ranked(tally(&rows), 2);
    let mut out = format!(
        "{NOTE}\nArtists tagged \"{tag}\" (work minted in the last 90 days) with the most different collectors in the last {days} days ({} sales):\n",
        rows.len()
    );
    for (a, st) in list.iter().take(n.clamp(1, 25)) {
        out += &format!(
            "- {} · {} collectors · {} sales, {} · last sale {} · {}\n",
            st.label,
            st.buyers.len(),
            st.sales,
            tez(st.volume),
            when(&st.last),
            profile_url(a)
        );
    }
    if list.is_empty() {
        out += "- no artist sold to 2+ collectors with that tag lately\n";
    }
    let want = n.clamp(1, 25);
    if list.len() < want {
        let shown: HashSet<String> = list.iter().take(want).map(|(a, _)| a.clone()).collect();
        out += &makers(tag, &shown, want - list.len().min(want))?;
    }
    out += "Tags are chosen by artists, so they're rough: look an artist up to see the work.\n";
    Ok(out)
}

/// Spellings artists use for one style: voxel → voxel, voxels, voxelart.
fn tag_variants(tag: &str) -> Vec<String> {
    let t = tag.to_lowercase();
    let stem = t.strip_suffix("art").filter(|s| s.len() >= 3).unwrap_or(&t).trim_end_matches(['-', '_', ' ']).to_string();
    let mut v = vec![tag.to_string(), t.clone(), stem.clone(), format!("{stem}art"), format!("{stem}s"), format!("{stem}-art"), format!("{stem} art")];
    let mut seen = HashSet::new();
    v.retain(|x| x.len() >= 2 && seen.insert(x.clone()));
    v
}

/// The wider search when few artists sold the style lately: everyone who ever
/// minted work tagged with any spelling of it, or with it in the title, ranked by
/// different collectors over the past year, then by how many such works they made.
fn makers(tag: &str, shown: &HashSet<String>, n: usize) -> Result<String, String> {
    let tags = tag_variants(tag);
    let low = tag.to_lowercase();
    let stem = low.strip_suffix("art").filter(|s| s.len() >= 3).unwrap_or(&low).trim_end_matches(['-', '_', ' ']).to_string();
    const WHO: &str = "pk timestamp creators { creator_address holder { alias tzdomain flag flag_reason } }";
    let tq = format!(
        "query($tags: [String!]!) {{ token(where: {{tags: {{tag: {{name: {{_in: $tags}}}}}}, supply: {{_gt: 0}}}}, \
         order_by: {{timestamp: desc}}, limit: 500) {{ {WHO} }} }}"
    );
    let mut found = gql(&tq, json!({ "tags": tags }))?["token"].as_array().cloned().unwrap_or_default();
    // titles too; unsorted, because sorting a title search times out on objkt
    let nq = format!("query($name: String!) {{ token(where: {{name: {{_ilike: $name}}, supply: {{_gt: 0}}}}, limit: 300) {{ {WHO} }} }}");
    if let Ok(d) = gql(&nq, json!({ "name": format!("%{stem}%") })) {
        found.extend(d["token"].as_array().cloned().unwrap_or_default());
    }
    // artist → (label, works, latest mint); and the works themselves
    let mut by: HashMap<String, (String, usize, String)> = HashMap::new();
    let mut pks: Vec<i64> = Vec::new();
    for t in found {
        let Some(pk) = num(&t["pk"]) else { continue };
        if pks.contains(&pk) {
            continue;
        }
        let Some(cr) = t["creators"].get(0) else { continue };
        let Some(a) = cr["creator_address"].as_str() else { continue };
        if a == BURN || a.starts_with("KT1") || !flag_of(&cr["holder"]).is_empty() || shown.contains(a) {
            continue;
        }
        pks.push(pk);
        let e = by.entry(a.to_string()).or_insert_with(|| (holder_label(&cr["holder"], a), 0, String::new()));
        e.1 += 1;
        let ts = t["timestamp"].as_str().unwrap_or("").to_string();
        if ts > e.2 {
            e.2 = ts;
        }
    }
    if by.is_empty() {
        return Ok(format!("No artist on objkt tagged or titled work \"{stem}\" (tried {}). Try a related style.\n", tags.join(", ")));
    }
    // who collected these very works (any date): the style's real audience
    // in batches: one query over hundreds of works times out on objkt
    let sq = format!(
        "query($p: [bigint!]!) {{ event(where: {{token_pk: {{_in: $p}}, {SALE_FILTER}}}, limit: 500) {{ {SALE_FIELDS} {CREATOR_FIELDS} }} }}"
    );
    let (mut events, mut failed) = (Vec::new(), 0);
    for chunk in pks.chunks(100) {
        match gql(&sq, json!({ "p": chunk })) {
            Ok(d) => events.extend(d["event"].as_array().cloned().unwrap_or_default()),
            Err(_) => failed += 1,
        }
    }
    let note = if failed > 0 { format!("(sales of {failed} batch(es) of works couldn't be loaded: counts are low)\n") } else { String::new() };
    let rows: Vec<(Value, Value)> = events
        .into_iter()
        .map(|e| (sale(&e), e["token"]["creators"].get(0).cloned().unwrap_or(Value::Null)))
        .collect();
    let stats = tally(&rows);
    let mut list: Vec<(String, usize, usize, i64, String, String)> = by
        .keys()
        .map(|a| {
            let (label, works, latest) = by[a].clone();
            let (buyers, vol) = stats.get(a).map_or((0, 0), |s| (s.buyers.len(), s.volume));
            (a.clone(), buyers, works, vol, label, latest)
        })
        .collect();
    list.sort_by(|x, y| y.1.cmp(&x.1).then(y.2.cmp(&x.2)));
    let mut out = format!(
        "\nMore artists who made \"{stem}\" work (tags {} or the title; any date), by different collectors of that work:\n",
        tags.join(", ")
    );
    out += &note;
    for (a, buyers, works, vol, label, latest) in list.iter().take(n) {
        out += &format!(
            "- {label} · {works} such work(s), latest minted {} · {buyers} collector(s), {} sold · {}\n",
            when(&Value::String(latest.clone())),
            tez(*vol),
            profile_url(a)
        );
    }
    Ok(out)
}

/// Artists collected by the same people who collect `who` (most shared collectors first).
pub fn similar(who: &str, n: usize) -> Result<String, String> {
    let w = crate::art::resolve(who)?;
    let col_q = "query($a: String!, $burn: String!, $off: Int!) { token_holder(where: {token: {creators: {creator_address: \
        {_eq: $a}}}, holder_address: {_nin: [$a, $burn]}, quantity: {_gt: 0}}, limit: 500, offset: $off, order_by: {token_pk: asc}) \
        { holder_address } }";
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in gql_pages(col_q, json!({ "a": w.address, "burn": BURN }), "token_holder", 2000)? {
        *counts.entry(r["holder_address"].as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by_key(|x| std::cmp::Reverse(x.1));
    let collectors: Vec<String> = top.into_iter().take(15).map(|(a, _)| a).collect();
    if collectors.is_empty() {
        return Ok(format!("{NOTE}\nNobody else holds work by {} yet, so there's nothing to compare.\n", w.label()));
    }
    // one query per collector (their 200 most recent pieces), so one huge collection can't crowd out the rest
    let hold_q = "query($h: String!) { token_holder(where: {holder_address: {_eq: $h}, quantity: {_gt: 0}, \
        token: {decimals: {_eq: 0}}}, order_by: {last_incremented_at: desc}, limit: 200) { holder_address \
        token { creators { creator_address holder { alias tzdomain flag flag_reason } } } } }";
    let mut rows = Vec::new();
    for c in &collectors {
        rows.extend(gql(hold_q, json!({ "h": c }))?["token_holder"].as_array().cloned().unwrap_or_default());
    }
    let mut shared: HashMap<String, (HashSet<String>, String)> = HashMap::new();
    for r in rows {
        let Some(cr) = r["token"]["creators"].get(0) else { continue };
        let Some(a) = cr["creator_address"].as_str() else { continue };
        if a == w.address || a == BURN || a.starts_with("KT1") || !flag_of(&cr["holder"]).is_empty() {
            continue;
        }
        let e = shared.entry(a.to_string()).or_insert_with(|| (HashSet::new(), holder_label(&cr["holder"], a)));
        e.0.insert(r["holder_address"].as_str().unwrap_or("").to_string());
    }
    let mut list: Vec<(String, (HashSet<String>, String))> = shared.into_iter().filter(|(_, (c, _))| c.len() >= 2).collect();
    list.sort_by_key(|x| std::cmp::Reverse(x.1 .0.len()));
    let mut out = format!(
        "{NOTE}\nArtists collected by the same people as {} (out of their {} main collectors):\n",
        w.label(),
        collectors.len()
    );
    for (a, (c, label)) in list.iter().take(n.clamp(1, 25)) {
        out += &format!("- {label} · {} shared collectors · {}\n", c.len(), profile_url(a));
    }
    if list.is_empty() {
        out += "- no artist shares 2+ collectors with them\n";
    }
    out += "Shared collectors suggest a similar audience, not a similar style: look an artist up to see the work.\n";
    Ok(out)
}

/// A collection (contract) by its KT1 address.
pub fn collection(spec: &str) -> Result<String, String> {
    let kt = spec
        .split(['/', ' ', ':', '?', '#'])
        .find(|p| p.starts_with("KT1") && crate::art::is_tez_address(p))
        .ok_or("give the collection's KT1… address or objkt link")?;
    let q = format!(
        "query($c: String!) {{ fa(where: {{contract: {{_eq: $c}}}}) {{ name description items owners editions floor_price \
         volume_total timestamp creator_address creator {{ alias tzdomain flag flag_reason }} }} \
         event(where: {{fa_contract: {{_eq: $c}}, {SALE_FILTER}}}, order_by: {{timestamp: desc}}, limit: 8) {{ {SALE_FIELDS} token {{ name }} }} }}"
    );
    let d = gql(&q, json!({ "c": kt }))?;
    let f = d["fa"].get(0).cloned().ok_or(format!("objkt has no collection {kt}"))?;
    let creator = f["creator_address"].as_str().unwrap_or("");
    let mut out = format!(
        "{NOTE}\nCollection: {} · https://objkt.com/collections/{kt}\nCreated by {} · {}\n",
        f["name"].as_str().filter(|s| !s.is_empty()).unwrap_or("(unnamed)"),
        holder_label(&f["creator"], creator),
        when(&f["timestamp"])
    );
    out += &format!(
        "{} items · {} editions · {} owners · total volume {}{}\n",
        num(&f["items"]).unwrap_or(0),
        num(&f["editions"]).unwrap_or(0),
        num(&f["owners"]).unwrap_or(0),
        tez(num(&f["volume_total"]).unwrap_or(0)),
        num(&f["floor_price"]).filter(|p| *p > 0).map(|p| format!(" · floor {}", tez(p))).unwrap_or_default()
    );
    let fl = flag_of(&f["creator"]);
    if !fl.is_empty() {
        out += &format!("⚠ objkt flagged the creator account ({fl})\n");
    }
    let sales: Vec<Value> = d["event"].as_array().map(|a| a.iter().map(sale).collect()).unwrap_or_default();
    if !sales.is_empty() {
        out += "Recent sales:\n";
        for (s, e) in sales.iter().zip(d["event"].as_array().unwrap_or(&Vec::new())) {
            out += &format!(
                "- {} · {} · {}\n",
                when(&s["timestamp"]),
                e["token"]["name"].as_str().unwrap_or("(untitled)"),
                tez(num(&s["price_xtz"]).unwrap_or(0))
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_dates() {
        let today = crate::util::fmt_utc_iso(crate::util::now());
        assert_eq!(days_ago(&json!(today)), Some(0));
        let ten = crate::util::fmt_utc_iso(crate::util::now() - 10 * 86_400);
        assert_eq!(days_ago(&json!(ten)), Some(10));
        assert!(when(&json!(ten)).ends_with("10 days ago"));
        assert_eq!(days_ago(&json!("garbage")), None);
    }

    #[test]
    fn names_compare_without_punctuation() {
        assert_eq!(norm("ilan_katin"), norm("Ilan Katin"));
        assert_eq!(norm("jamesbloom"), norm("James Bloom"));
        assert_ne!(norm("crashblossom"), norm("James Bloom"));
    }
}
