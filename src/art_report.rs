//! `art action=report`: a PDF of chosen artworks with their images and market
//! numbers, made in one step. The images are downloaded here and embedded in the
//! page, so the PDF never depends on the browser reaching IPFS (on a Pi it
//! often can't), and the numbers come straight from objkt, never from memory.

use crate::art::{
    day, fetch_image, gql, holder_label, image_urls, num, parse_piece, piece_url, profile_url, sale, tez, BURN,
    SALE_FIELDS, SALE_FILTER,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_PIECES: usize = 24;
/// Image bytes embedded in one report; past this, pieces use their small thumbnail.
const IMAGE_BUDGET: usize = 40 * 1024 * 1024;

fn piece_q() -> String {
    format!(
        "query($c: String!, $t: String!) {{ token(where: {{fa_contract: {{_eq: $c}}, token_id: {{_eq: $t}}}}) {{ \
         name mime supply timestamp lowest_ask display_uri thumbnail_uri \
         creators {{ creator_address holder {{ alias tzdomain }} }} \
         holders(where: {{quantity: {{_gt: 0}}}}, limit: 1000) {{ holder_address quantity }} \
         listings_active(order_by: {{price_xtz: asc}}, limit: 50) {{ price_xtz amount_left seller_address }} \
         events(where: {{{SALE_FILTER}}}, order_by: {{timestamp: desc}}, limit: 500) {{ {SALE_FIELDS} }} }} }}"
    )
}

/// An artist's works that sold best (most sales, then newest), for `who` lists.
const BEST_Q: &str = "query($a: String!) { token(where: {creators: {creator_address: {_eq: $a}}, supply: {_gt: 0}}, \
    order_by: {timestamp: desc}, limit: 30) { fa_contract token_id events(where: {price_xtz: {_gt: 0}, \
    marketplace_event_type: {_in: [\"list_buy\", \"english_auction_settle\", \"dutch_auction_buy\", \"offer_accept\"]}}, limit: 200) { price_xtz } } }";

/// Everything the page and the summary show about one piece.
struct Piece {
    name: String,
    url: String,
    artist: String,
    artist_url: String,
    minted: String,
    supply: i64,
    holders: usize,
    artist_holds: i64,
    mime: String,
    floor: Option<i64>,
    listed: i64,
    primary_left: i64,
    primary_price: Option<i64>,
    sales: usize,
    sales_30d: usize,
    volume: i64,
    last: Option<(String, i64)>,
    high: Option<i64>,
    image: Option<String>,
}

impl Piece {
    fn status(&self) -> String {
        if self.primary_left > 0 {
            format!(
                "primary open: {} of {} left at {}",
                self.primary_left,
                self.supply,
                self.primary_price.map(tez).unwrap_or_default()
            )
        } else if self.artist_holds == 0 {
            "sold out".into()
        } else {
            format!("artist keeps {} (not listed)", self.artist_holds)
        }
    }
    fn market(&self) -> &'static str {
        match (self.sales_30d, self.sales) {
            (s, _) if s >= 5 => "active (5+ sales in 30 days)",
            (s, _) if s >= 1 => "some sales in the last 30 days",
            (_, 0) => "never sold",
            _ => "no sales in the last 30 days",
        }
    }
}

fn load(c: &str, id: &str, budget: &mut usize) -> Result<Piece, String> {
    let d = gql(&piece_q(), json!({ "c": c, "t": id }))?;
    let t = d["token"].get(0).cloned().ok_or(format!("no token {id} on contract {c}"))?;
    let creator = t["creators"].get(0).cloned().unwrap_or(Value::Null);
    let ca = creator["creator_address"].as_str().unwrap_or("").to_string();
    // people only: marketplace contracts (KT1…) hold listed editions in escrow, and burned ones count for no one
    let holders: Vec<Value> = t["holders"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|h| h["holder_address"].as_str().is_some_and(|a| a != BURN && !a.starts_with("KT1")))
        .collect();
    let listings = t["listings_active"].as_array().cloned().unwrap_or_default();
    let sales: Vec<Value> = t["events"].as_array().map(|a| a.iter().map(sale).collect()).unwrap_or_default();
    let month_ago = crate::util::fmt_utc_iso(crate::util::now().saturating_sub(30 * 86_400));
    let primary: Vec<&Value> = listings.iter().filter(|l| l["seller_address"].as_str() == Some(&ca)).collect();
    let image = embed(&t, c, id, budget);
    Ok(Piece {
        name: t["name"].as_str().filter(|s| !s.is_empty()).unwrap_or("(untitled)").to_string(),
        url: piece_url(c, id),
        artist: holder_label(&creator["holder"], &ca),
        artist_url: profile_url(&ca),
        minted: day(&t["timestamp"]),
        supply: num(&t["supply"]).unwrap_or(0),
        holders: holders.len(),
        artist_holds: holders
            .iter()
            .filter(|h| h["holder_address"].as_str() == Some(&ca))
            .filter_map(|h| num(&h["quantity"]))
            .sum(),
        mime: t["mime"].as_str().unwrap_or("").to_string(),
        floor: num(&t["lowest_ask"]),
        listed: listings.iter().filter_map(|l| num(&l["amount_left"])).sum(),
        primary_left: primary.iter().filter_map(|l| num(&l["amount_left"])).sum(),
        primary_price: primary.first().and_then(|l| num(&l["price_xtz"])),
        sales: sales.len(),
        sales_30d: sales.iter().filter(|s| s["timestamp"].as_str().unwrap_or("") >= month_ago.as_str()).count(),
        volume: sales.iter().filter_map(|s| num(&s["price_xtz"])).sum(),
        last: sales.first().map(|s| (day(&s["timestamp"]), num(&s["unit_xtz"]).unwrap_or(0))),
        high: sales.iter().filter_map(|s| num(&s["unit_xtz"])).max(),
        image,
    })
}

/// The piece's picture as a data: URL: the display image, or the thumbnail when
/// that is too big, won't load, or the report's image budget is spent.
fn embed(t: &Value, c: &str, id: &str, budget: &mut usize) -> Option<String> {
    let keys: &[&str] = if *budget > 0 { &["display_uri", "thumbnail_uri"] } else { &["thumbnail_uri"] };
    let mut tries: Vec<Vec<String>> =
        keys.iter().map(|k| image_urls(t[*k].as_str().unwrap_or(""))).filter(|u| !u.is_empty()).collect();
    // objkt's own resized copies, for pieces whose images are huge (an 86 MB animation)
    for size in ["thumb400", "thumb288"] {
        tries.push(vec![format!("https://assets.objkt.media/file/assets-003/{c}/{id}/{size}")]);
    }
    for urls in tries {
        if let Ok(bytes) = fetch_image(&urls) {
            if let Ok(url) = crate::util::image_data_url(&bytes) {
                *budget = budget.saturating_sub(bytes.len());
                return Some(url);
            }
        }
    }
    None
}

/// The pieces to show: `pieces` (links or KT1…/id, as a list or comma separated),
/// and/or `who` (artists, comma separated: their best-selling works, `per` each).
fn choose(a: &Value) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let list = |k: &str| -> Vec<String> {
        match &a[k] {
            Value::Array(v) => v.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            Value::String(s) => s.split([',', '\n']).map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            _ => Vec::new(),
        }
    };
    for p in list("pieces") {
        out.push(parse_piece(&p).ok_or(format!("not a piece: {p} (use an objkt link or KT1…/id)"))?);
    }
    let per = a["per"].as_u64().unwrap_or(3).clamp(1, 8) as usize;
    for w in list("who") {
        let addr = crate::art::resolve(&w)?.address;
        let rows = gql(BEST_Q, json!({ "a": addr }))?["token"].as_array().cloned().unwrap_or_default();
        let mut ranked: Vec<(usize, usize, (String, String))> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let sold = r["events"].as_array().map_or(0, |e| e.len());
                Some((sold, i, (r["fa_contract"].as_str()?.to_string(), r["token_id"].as_str()?.to_string())))
            })
            .collect();
        ranked.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        out.extend(ranked.into_iter().take(per).map(|r| r.2));
    }
    out.dedup();
    if out.is_empty() {
        return Err("give pieces= (objkt links or KT1…/id) and/or who= (artists, comma separated)".into());
    }
    out.truncate(MAX_PIECES);
    Ok(out)
}

/// "image/webp" → "image (webp)", "application/x-directory" → "interactive (HTML)".
fn kind(mime: &str) -> String {
    match mime.split_once('/') {
        Some(("image", f)) => format!("image ({})", f.trim_start_matches("svg+").trim_end_matches("+xml")),
        Some(("video", f)) => format!("video ({f})"),
        Some(("audio", f)) => format!("audio ({f})"),
        Some(("model", _)) => "3D model".into(),
        Some(("application", "x-directory" | "html")) | Some(("text", "html")) => "interactive (HTML)".into(),
        Some(("application", "pdf")) => "PDF".into(),
        _ if mime.is_empty() => "unknown".into(),
        _ => esc(mime),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(title: &str, pieces: &[Piece]) -> String {
    let today = crate::util::fmt_utc_iso(crate::util::now()).chars().take(10).collect::<String>();
    let mut h = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title><style>\
         body{{font:14px/1.45 -apple-system,'Segoe UI',Helvetica,Arial,sans-serif;color:#111;max-width:860px;margin:24px auto;padding:0 16px}}\
         h1{{font-size:26px;margin:0 0 4px}} .sub{{color:#666;margin:0 0 18px}}\
         table{{border-collapse:collapse;width:100%;font-size:11.5px}} th,td{{border:1px solid #ddd;padding:5px 6px;text-align:left;vertical-align:top}}\
         th{{background:#f4f4f4}} .card{{break-inside:avoid;page-break-inside:avoid;margin:26px 0;border-top:2px solid #111;padding-top:12px}}\
         .card img{{display:block;max-width:100%;max-height:640px;margin:8px auto;object-fit:contain}} .noimg{{color:#999;padding:40px;text-align:center;border:1px dashed #ccc}}\
         .card h2{{font-size:19px;margin:0}} a{{color:#111}} .stats td:first-child{{width:34%;color:#555}} .foot{{color:#777;font-size:11px;margin-top:30px}}\
         </style></head><body><h1>{t}</h1><p class=\"sub\">{n} artworks · data from objkt.com on {today}</p>\
         <table><tr><th>Artwork</th><th>Artist</th><th>Minted</th><th>Ed.</th><th>Floor</th><th>Sales</th><th>Volume</th><th>Last sale</th><th>Status</th></tr>",
        t = esc(title),
        n = pieces.len()
    );
    for p in pieces {
        h += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&p.name),
            esc(&p.artist),
            p.minted,
            p.supply,
            p.floor.map(tez).unwrap_or("—".into()),
            p.sales,
            tez(p.volume),
            p.last.as_ref().map(|(d, v)| format!("{} ({d})", tez(*v))).unwrap_or("—".into()),
            esc(&p.status())
        );
    }
    h += "</table>";
    for p in pieces {
        let img = match &p.image {
            Some(u) => format!("<img src=\"{u}\" alt=\"{}\">", esc(&p.name)),
            None => "<div class=\"noimg\">no preview image could be downloaded</div>".into(),
        };
        let row = |k: &str, v: String| format!("<tr><td>{k}</td><td>{v}</td></tr>");
        h += &format!(
            "<div class=\"card\"><h2>{}</h2><div>by <a href=\"{}\">{}</a> · <a href=\"{}\">objkt</a></div>{img}<table class=\"stats\">{}</table></div>",
            esc(&p.name),
            p.artist_url,
            esc(&p.artist),
            p.url,
            [
                row("Minted", p.minted.clone()),
                row("Edition", format!("{} · {} holder(s)", p.supply, p.holders)),
                row("Primary", esc(&p.status())),
                row("Floor now", p.floor.map(|f| format!("{} · {} edition(s) listed", tez(f), p.listed)).unwrap_or("not listed".into())),
                row("Sales", format!("{} total · {} in the last 30 days · {}", p.sales, p.sales_30d, p.market())),
                row("Volume", tez(p.volume)),
                row("Last sale", p.last.as_ref().map(|(d, v)| format!("{} on {d}", tez(*v))).unwrap_or("never sold".into())),
                row("Highest sale", p.high.map(tez).unwrap_or("—".into())),
                row("Format", kind(&p.mime)),
            ]
            .concat()
        );
    }
    h + "<p class=\"foot\">Prices per edition in tez. Sales = fixed price, auctions and accepted offers on objkt. Not financial advice.</p></body></html>"
}

/// A browser that can print to PDF.
fn find_chromium() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    for app in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ] {
        if Path::new(app).is_file() {
            return Some(app.into());
        }
    }
    let path = std::env::var_os("PATH")?;
    ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "brave-browser", "microsoft-edge"]
        .iter()
        .find_map(|n| std::env::split_paths(&path).map(|d| d.join(n)).find(|p| p.is_file()))
}

/// Print the page to PDF with headless Chromium: its own fresh profile (so it never
/// joins a running browser, like a kiosk), lowest priority, killed after a minute.
fn print_pdf(html: &Path, pdf: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let exe = find_chromium().ok_or("no Chromium-family browser to print the PDF")?;
    let profile = std::env::temp_dir().join(format!("pagurus-pdf-{}", std::process::id()));
    let _ = std::fs::remove_file(pdf);
    let nice = ["/usr/bin/nice", "/bin/nice"].into_iter().find(|p| Path::new(p).is_file());
    let mut cmd = match nice {
        Some(n) => {
            let mut c = Command::new(n);
            c.args(["-n", "19"]).arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    cmd.args([
        "--headless",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--no-pdf-header-footer",
        "--hide-scrollbars",
    ])
    .arg(format!("--user-data-dir={}", profile.display()))
    .arg(format!("--print-to-pdf={}", pdf.display()))
    .arg(format!("file://{}", html.display()))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    crate::tools::clean_env(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start {}: {e}", exe.display()))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let done = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            _ => break false,
        }
    };
    #[cfg(unix)]
    if !done {
        // the whole group: Chromium leaves helper processes behind
        let _ = Command::new("kill").args(["-9", &format!("-{}", child.id())]).status();
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);
    match std::fs::metadata(pdf) {
        Ok(m) if m.len() > 1024 => Ok(()),
        _ if !done => Err("Chromium took longer than a minute".into()),
        _ => Err("Chromium didn't write the PDF".into()),
    }
}

fn slug(s: &str) -> String {
    let mut out: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out: String = out.trim_matches('-').chars().take(50).collect();
    if out.is_empty() {
        "art-report".into()
    } else {
        out
    }
}

pub fn report(home: &Path, a: &Value) -> Result<String, String> {
    let chosen = choose(a)?;
    let title = a["title"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or("Art report").trim().to_string();
    let mut budget = IMAGE_BUDGET;
    let mut pieces = Vec::new();
    let mut skipped = Vec::new();
    for (c, id) in &chosen {
        match load(c, id, &mut budget) {
            Ok(p) => pieces.push(p),
            Err(e) => skipped.push(format!("{c}/{id}: {e}")),
        }
    }
    if pieces.is_empty() {
        return Err(format!("no piece could be loaded: {}", skipped.join("; ")));
    }
    let dir = home.join("workspace").join("reports");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = dir.join(slug(&title));
    let html = base.with_extension("html");
    std::fs::write(&html, page(&title, &pieces)).map_err(|e| e.to_string())?;
    let pdf = base.with_extension("pdf");
    // DevTools first (headless --print-to-pdf hangs in some browsers, e.g. Brave), the command line second
    #[cfg(feature = "browser")]
    let printed = crate::browser::print_to_pdf(home, &html, &pdf)
        .or_else(|e| print_pdf(&html, &pdf).map_err(|e2| format!("{e}; {e2}")));
    #[cfg(not(feature = "browser"))]
    let printed = print_pdf(&html, &pdf);
    let (file, made) = match printed {
        Ok(()) => {
            let _ = std::fs::remove_file(&html); // the images make it big; the PDF has it all
            (pdf, "PDF".to_string())
        }
        Err(e) => (html.clone(), format!("HTML page (no PDF: {e})")),
    };
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    crate::tools::attach(file.clone(), title.clone());

    let mut out = format!(
        "{}\nMade a {made} with {} artworks, {} with their image: {} ({} KB). It is attached to your reply already; \
         don't call send_file for it. Tell the user only what is below; these are all the numbers there are.\n",
        crate::art::NOTE,
        pieces.len(),
        pieces.iter().filter(|p| p.image.is_some()).count(),
        file.display(),
        size / 1024
    );
    for p in &pieces {
        out += &format!(
            "- {} by {} · minted {} · ed. {} · {} · floor {} · {} sales ({} in 30 days), volume {} · last sale {}{}\n",
            p.name,
            p.artist,
            p.minted,
            p.supply,
            p.status(),
            p.floor.map(tez).unwrap_or("not listed".into()),
            p.sales,
            p.sales_30d,
            tez(p.volume),
            p.last.as_ref().map(|(d, v)| format!("{} on {d}", tez(*v))).unwrap_or("never".into()),
            if p.image.is_none() { " · no image" } else { "" }
        );
    }
    if !skipped.is_empty() {
        out += &format!("Left out (couldn't load): {}\n", skipped.join("; "));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_and_artists_are_both_accepted_and_capped() {
        let a = json!({ "pieces": "https://objkt.com/tokens/KT1XaCf6gkjFnKg3QmPfn6gep53moMvjkj1E/50832, KT1XaCf6gkjFnKg3QmPfn6gep53moMvjkj1E/50978" });
        assert_eq!(choose(&a).unwrap().len(), 2);
        let a =
            json!({ "pieces": ["KT1XaCf6gkjFnKg3QmPfn6gep53moMvjkj1E/1", "KT1XaCf6gkjFnKg3QmPfn6gep53moMvjkj1E/1"] });
        assert_eq!(choose(&a).unwrap().len(), 1, "duplicates are shown once");
        assert!(choose(&json!({ "pieces": "pixel art" })).is_err());
        assert!(choose(&json!({})).is_err());
    }

    #[test]
    fn the_status_says_sold_out_open_or_kept() {
        let mut p = Piece {
            name: "x".into(),
            url: String::new(),
            artist: String::new(),
            artist_url: String::new(),
            minted: String::new(),
            supply: 100,
            holders: 3,
            artist_holds: 0,
            mime: String::new(),
            floor: None,
            listed: 0,
            primary_left: 0,
            primary_price: None,
            sales: 0,
            sales_30d: 0,
            volume: 0,
            last: None,
            high: None,
            image: None,
        };
        assert_eq!(p.status(), "sold out");
        assert_eq!(p.market(), "never sold");
        p.primary_left = 98;
        p.primary_price = Some(2_000_000);
        assert!(p.status().starts_with("primary open: 98 of 100 left at 2"));
        p.primary_left = 0;
        p.artist_holds = 5;
        assert_eq!(p.status(), "artist keeps 5 (not listed)");
        p.sales = 9;
        p.sales_30d = 6;
        assert!(p.market().starts_with("active"));
    }

    #[test]
    fn the_page_escapes_names_and_embeds_images() {
        let p = Piece {
            name: "<b>evil</b>".into(),
            url: "u".into(),
            artist: "A & B".into(),
            artist_url: "p".into(),
            minted: "2026-11-19".into(),
            supply: 1,
            holders: 1,
            artist_holds: 0,
            mime: "image/png".into(),
            floor: Some(1_500_000),
            listed: 1,
            primary_left: 0,
            primary_price: None,
            sales: 2,
            sales_30d: 1,
            volume: 3_000_000,
            last: Some(("2026-11-20".into(), 1_500_000)),
            high: Some(1_500_000),
            image: Some("data:image/png;base64,AAAA".into()),
        };
        let h = page("Pixel <report>", &[p]);
        assert!(h.contains("&lt;b&gt;evil") && !h.contains("<b>evil"));
        assert!(h.contains("A &amp; B") && h.contains("Pixel &lt;report&gt;"));
        assert!(h.contains("<img src=\"data:image/png;base64,AAAA\""));
        assert!(h.contains("1.5 ꜩ on 2026-11-20"));
        assert!(h.contains("image (png)"));
        assert_eq!(kind("application/x-directory"), "interactive (HTML)");
        assert_eq!(kind("model/gltf-binary"), "3D model");
    }
}
