//! Deterministic natural-language query parsing. No language model: dates,
//! file kinds and scope are recognised by a small grammar, and everything
//! left over is the semantic part handed to the embedders.
//!
//! "all photos from december last year"  -> kinds [image], Dec 2025, semantic ""
//! "sales report for 25's last quarter"  -> Q4 2025, semantic "sales report"
//! "script that runs terraform on aws"   -> kinds [code], semantic "runs terraform aws"

use jiff::civil::Date;
use jiff::{ToSpan, Zoned};
use serde::{Deserialize, Serialize};

use crate::classify::Category;
use crate::timeutil::CivilRange;

#[derive(Debug, Clone, Default)]
pub struct ParsedQuery {
    pub kinds: Vec<Category>,
    pub range: Option<CivilRange>,
    /// Human readable description of the recognised date, e.g. "December 2025".
    pub range_label: Option<String>,
    /// Remaining meaningful words, original casing, for the embedders.
    pub semantic: String,
    /// Lower-cased keywords for full-text search (semantic words + date words).
    pub terms: Vec<String>,
    /// Query asked to stay inside the current folder ("here", "this project").
    pub scope_here: bool,
    /// Possible folder references ("from downloads", "in my zed folder"),
    /// longest first. Each carries the query as parsed without those words;
    /// the daemon uses the first one that names exactly one folder.
    pub places: Vec<Place>,
}

#[derive(Debug, Clone)]
pub struct Place {
    pub phrase: String,
    pub without: Box<ParsedQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSummary {
    pub kinds: Vec<Category>,
    pub date: Option<String>,
    pub semantic: String,
    pub scope_here: bool,
    /// Folder the query was limited to ("~/Downloads"), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl ParsedQuery {
    pub fn summary(&self) -> ParsedSummary {
        ParsedSummary { kinds: self.kinds.clone(), date: self.range_label.clone(), semantic: self.semantic.clone(), scope_here: self.scope_here, scope: None }
    }
}

const MONTHS: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

fn month_of(t: &str) -> Option<i8> {
    let m = match t {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    };
    Some(m)
}

fn year4(t: &str) -> Option<i16> {
    if t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit()) {
        let y: i16 = t.parse().ok()?;
        if (1990..=2100).contains(&y) {
            return Some(y);
        }
    }
    None
}

/// Year in a date context: 4 digits, "'25", or bare 2 digits when `ctx`.
fn year_ctx(t: &str, ctx: bool) -> Option<i16> {
    if let Some(y) = year4(t) {
        return Some(y);
    }
    let (digits, explicit) = match t.strip_prefix('\'') {
        Some(d) => (d, true),
        None => (t, false),
    };
    if (explicit || ctx) && digits.len() == 2 && digits.bytes().all(|b| b.is_ascii_digit()) {
        return Some(2000 + digits.parse::<i16>().ok()?);
    }
    None
}

fn kind_of(t: &str) -> Option<&'static [Category]> {
    use Category::*;
    Some(match t {
        "photo" | "photos" | "pic" | "pics" | "picture" | "pictures" | "image" | "images" | "screenshot" | "screenshots" | "selfie" | "selfies" | "jpg" | "jpeg" | "png" => &[Image],
        "video" | "videos" | "movie" | "movies" | "footage" | "mp4" => &[Video],
        "song" | "songs" | "music" | "audio" | "podcast" | "podcasts" | "mp3" => &[Audio],
        "pdf" | "pdfs" => &[Pdf],
        "document" | "documents" | "doc" | "docs" | "docx" => &[Document, Pdf, Text],
        "slides" | "slide" | "presentation" | "presentations" | "deck" | "decks" | "ppt" | "pptx" => &[Presentation, Pdf],
        "spreadsheet" | "spreadsheets" | "sheet" | "sheets" | "excel" | "xlsx" | "csv" => &[Spreadsheet],
        "script" | "scripts" | "code" | "function" | "functions" | "method" | "methods" | "class" | "snippet" | "source" => &[Code],
        "cad" | "stl" | "step" | "dxf" => &[Cad],
        "design" | "designs" | "mockup" | "mockups" | "psd" | "figma" | "svg" => &[Design],
        "archive" | "archives" | "zip" | "tarball" => &[Archive],
        "folder" | "folders" | "directory" | "directories" => &[Dir],
        _ => return None,
    })
}

const STOP: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "for", "from", "to", "with", "by", "and", "or", "my", "me", "i", "all", "any",
    "some", "find", "show", "get", "give", "list", "search", "where", "which", "that", "this", "these", "those", "is",
    "are", "was", "were", "be", "been", "it", "its", "file", "files", "stuff", "thing", "things", "about", "please", "can",
    "you", "want", "need", "look", "looking", "what", "whats", "did", "do", "does", "had", "has", "have", "we", "our", "us",
    "taken", "made", "created", "during", "around", "into", "one", "s",
];

#[derive(Clone, Copy, PartialEq)]
enum Used {
    No,
    Date,
    Kind,
    Scope,
}

struct Tok {
    orig: String,
    low: String,
}

fn tokenize(q: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    for raw in q.split_whitespace() {
        let trimmed = raw.trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-' || c == '/' || c == '_' || c == '.'));
        let trimmed = trimmed.trim_end_matches('.');
        // "25's" -> "25", "client's" -> "client"
        let trimmed = trimmed.strip_suffix("'s").or_else(|| trimmed.strip_suffix("’s")).unwrap_or(trimmed);
        let trimmed = if trimmed.starts_with('\'') && trimmed.len() == 3 { trimmed } else { trimmed.trim_matches('\'') };
        if trimmed.is_empty() {
            continue;
        }
        out.push(Tok { orig: trimmed.to_string(), low: trimmed.to_lowercase() });
    }
    out
}

fn month_range(y: i16, m: i8) -> Option<CivilRange> {
    let start = Date::new(y, m, 1).ok()?;
    let end = start.checked_add(1.month()).ok()?;
    Some(CivilRange::days(start, end))
}

fn quarter_range(y: i16, q: i8) -> Option<CivilRange> {
    let start = Date::new(y, (q - 1) * 3 + 1, 1).ok()?;
    let end = start.checked_add(3.months()).ok()?;
    Some(CivilRange::days(start, end))
}

fn year_range(y: i16) -> Option<CivilRange> {
    Some(CivilRange::days(Date::new(y, 1, 1).ok()?, Date::new(y + 1, 1, 1).ok()?))
}

fn week_start(d: Date) -> Date {
    let back = d.weekday().to_monday_zero_offset() as i64;
    d.checked_sub(back.days()).unwrap_or(d)
}

fn quarter_of(m: i8) -> i8 {
    (m - 1) / 3 + 1
}

fn ordinal_quarter(t: &str) -> Option<i8> {
    Some(match t {
        "first" | "1st" | "q1" => 1,
        "second" | "2nd" | "q2" => 2,
        "third" | "3rd" | "q3" => 3,
        "fourth" | "4th" | "last" | "q4" => 4,
        _ => return None,
    })
}

fn month_label(y: i16, m: i8) -> String {
    let name = MONTHS[(m - 1) as usize];
    let mut c = name.chars();
    let first = c.next().map(|f| f.to_ascii_uppercase()).unwrap_or('?');
    format!("{}{} {}", first, c.as_str(), y)
}

const PREPOSITIONS: &[&str] = &["from", "in", "inside", "under", "within"];
const POSSESSIVES: &[&str] = &["my", "the", "our", "your"];
const FOLDER_WORDS: &[&str] = &["folder", "directory", "dir", "folders"];

pub fn parse(query: &str, now: &Zoned) -> ParsedQuery {
    parse_inner(query, now, &[], true)
}

fn parse_inner(query: &str, now: &Zoned, mask: &[usize], find_places: bool) -> ParsedQuery {
    let toks = tokenize(query);
    let n = toks.len();
    let mut used = vec![Used::No; n];
    for &m in mask {
        if m < n {
            used[m] = Used::Scope;
        }
    }
    let today = now.date();
    let mut out = ParsedQuery::default();
    let mut date_terms: Vec<String> = Vec::new();

    let low = |i: usize| -> &str { toks.get(i).map(|t| t.low.as_str()).unwrap_or("") };

    let set = |out: &mut ParsedQuery, used: &mut Vec<Used>, r: Option<CivilRange>, label: String, span: std::ops::Range<usize>| -> bool {
        if out.range.is_some() {
            return false;
        }
        if let Some(r) = r {
            out.range = Some(r);
            out.range_label = Some(label);
            for u in &mut used[span] {
                *u = Used::Date;
            }
            return true;
        }
        false
    };

    let mut i = 0;
    while i < n {
        let t = low(i);
        let next = low(i + 1);
        let next2 = low(i + 2);
        let prev_year = if i > 0 && used[i - 1] == Used::No { year_ctx(low(i - 1), true) } else { None };

        // ---- quarters -------------------------------------------------
        if (t == "last" || t == "past" || t == "previous") && (next == "quarter" || next == "qtr") {
            // "25's last quarter", "last quarter of 2025", or relative.
            if let Some(y) = prev_year {
                if set(&mut out, &mut used, quarter_range(y, 4), format!("Q4 {y}"), i - 1..i + 2) {
                    date_terms.extend(["q4".into(), y.to_string()]);
                }
            } else if next2 == "of" && year_ctx(low(i + 3), true).is_some() {
                let y = year_ctx(low(i + 3), true).unwrap();
                if set(&mut out, &mut used, quarter_range(y, 4), format!("Q4 {y}"), i..i + 4) {
                    date_terms.extend(["q4".into(), y.to_string()]);
                }
            } else {
                let cq = quarter_of(today.month());
                let (y, q) = if cq == 1 { (today.year() - 1, 4) } else { (today.year(), cq - 1) };
                if set(&mut out, &mut used, quarter_range(y, q), format!("Q{q} {y}"), i..i + 2) {
                    date_terms.extend([format!("q{q}"), y.to_string()]);
                }
            }
            i += 2;
            continue;
        }
        if (t == "this" || t == "current") && (next == "quarter" || next == "qtr") {
            let q = quarter_of(today.month());
            let y = today.year();
            if set(&mut out, &mut used, quarter_range(y, q), format!("Q{q} {y}"), i..i + 2) {
                date_terms.extend([format!("q{q}"), y.to_string()]);
            }
            i += 2;
            continue;
        }
        if let Some(q) = ordinal_quarter(t).filter(|_| next == "quarter" && t != "last") {
            // "fourth quarter of 2025", "2025 fourth quarter"
            let (y, span) = if next2 == "of" && year_ctx(low(i + 3), true).is_some() {
                (year_ctx(low(i + 3), true).unwrap(), i..i + 4)
            } else if let Some(y) = prev_year {
                (y, i - 1..i + 2)
            } else {
                (today.year(), i..i + 2)
            };
            if set(&mut out, &mut used, quarter_range(y, q), format!("Q{q} {y}"), span) {
                date_terms.extend([format!("q{q}"), y.to_string()]);
            }
            i += 2;
            continue;
        }
        if t.len() >= 2 && t.starts_with('q') && matches!(t.as_bytes()[1], b'1'..=b'4') {
            // q4, q4-25, q4'25, q4/2025, then optional following year token
            let q = (t.as_bytes()[1] - b'0') as i8;
            let rest = t[2..].trim_start_matches(['-', '\'', '/', '_']);
            let (y, span) = if !rest.is_empty() {
                match year_ctx(rest, true) {
                    Some(y) => (Some(y), i..i + 1),
                    None => (None, i..i + 1),
                }
            } else if let Some(y) = year_ctx(next, true) {
                (Some(y), i..i + 2)
            } else if let Some(y) = prev_year {
                (Some(y), i - 1..i + 1)
            } else if t.len() == 2 {
                (Some(today.year()), i..i + 1)
            } else {
                (None, i..i + 1)
            };
            if let Some(y) = y {
                if set(&mut out, &mut used, quarter_range(y, q), format!("Q{q} {y}"), span.clone()) {
                    date_terms.extend([format!("q{q}"), y.to_string()]);
                }
                i = span.end;
                continue;
            }
        }

        // ---- months ---------------------------------------------------
        let month_here = month_of(t).filter(|_| {
            // "may" is too often a verb; require a date neighbour for it.
            t != "may" || year_ctx(next, true).is_some() || next == "last" || next == "this" || matches!(low(i.wrapping_sub(1)), "last" | "this" | "in")
        });
        if (t == "last" || t == "this") && month_of(next).is_some() {
            let m = month_of(next).unwrap();
            // "this december" = this year's; "last december" = the most recent
            // one strictly before the current month.
            let y = if t == "this" || m < today.month() { today.year() } else { today.year() - 1 };
            if set(&mut out, &mut used, month_range(y, m), month_label(y, m), i..i + 2) {
                date_terms.extend([MONTHS[(m - 1) as usize].to_string(), y.to_string()]);
            }
            i += 2;
            continue;
        }
        if let Some(m) = month_here {
            let (y, span) = if let Some(y) = year_ctx(next, true) {
                (y, i..i + 2)
            } else if next == "last" && next2 == "year" {
                (today.year() - 1, i..i + 3)
            } else if next == "this" && next2 == "year" {
                (today.year(), i..i + 3)
            } else if (next == "of" || next == "in") && year_ctx(next2, true).is_some() {
                (year_ctx(next2, true).unwrap(), i..i + 3)
            } else {
                // Bare month: most recent one that is not in the future.
                let y = if m <= today.month() { today.year() } else { today.year() - 1 };
                (y, i..i + 1)
            };
            if set(&mut out, &mut used, month_range(y, m), month_label(y, m), span.clone()) {
                date_terms.extend([MONTHS[(m - 1) as usize].to_string(), y.to_string()]);
            }
            i = span.end;
            continue;
        }

        // ---- relative units -------------------------------------------
        if (t == "last" || t == "this" || t == "previous") && matches!(next, "year" | "month" | "week") {
            let last = t != "this";
            let (r, label) = match next {
                "year" => {
                    let y = if last { today.year() - 1 } else { today.year() };
                    (year_range(y), y.to_string())
                }
                "month" => {
                    let first = today.first_of_month();
                    let start = if last { first.checked_sub(1.month()).ok() } else { Some(first) };
                    let r = start.and_then(|s| month_range(s.year(), s.month()));
                    let label = start.map(|s| month_label(s.year(), s.month())).unwrap_or_default();
                    (r, label)
                }
                _ => {
                    let ws = week_start(today);
                    let start = if last { ws.checked_sub(7.days()).ok() } else { Some(ws) };
                    let r = start.and_then(|s| Some(CivilRange::days(s, s.checked_add(7.days()).ok()?)));
                    (r, if last { "last week".into() } else { "this week".into() })
                }
            };
            set(&mut out, &mut used, r, label, i..i + 2);
            i += 2;
            continue;
        }
        if t == "today" || t == "yesterday" {
            let d = if t == "today" { Some(today) } else { today.checked_sub(1.day()).ok() };
            if let Some(d) = d {
                let r = d.checked_add(1.day()).ok().map(|e| CivilRange::days(d, e));
                set(&mut out, &mut used, r, t.to_string(), i..i + 1);
            }
            i += 1;
            continue;
        }
        // "past 3 weeks", "last 10 days"
        if matches!(t, "past" | "last") && next.parse::<u32>().is_ok() && matches!(next2.trim_end_matches('s'), "day" | "week" | "month" | "year") {
            let k: i64 = next.parse::<i64>().unwrap().min(100);
            let span = match next2.trim_end_matches('s') {
                "day" => k.days(),
                "week" => (k * 7).days(),
                "month" => k.months(),
                _ => k.years(),
            };
            if let (Ok(start), Ok(end)) = (today.checked_sub(span), today.checked_add(1.day())) {
                set(&mut out, &mut used, Some(CivilRange::days(start, end)), format!("past {k} {next2}"), i..i + 3);
            }
            i += 3;
            continue;
        }
        // "3 weeks ago"
        if t.parse::<u32>().is_ok() && matches!(next.trim_end_matches('s'), "day" | "week" | "month" | "year") && next2 == "ago" {
            let k: i64 = t.parse::<i64>().unwrap().min(100);
            let unit = next.trim_end_matches('s');
            let anchor = match unit {
                "day" => today.checked_sub(k.days()),
                "week" => today.checked_sub((k * 7).days()),
                "month" => today.checked_sub(k.months()),
                _ => today.checked_sub(k.years()),
            };
            if let Ok(a) = anchor {
                let r = match unit {
                    "day" => a.checked_add(1.day()).ok().map(|e| CivilRange::days(a, e)),
                    "week" => {
                        let s = week_start(a);
                        s.checked_add(7.days()).ok().map(|e| CivilRange::days(s, e))
                    }
                    "month" => month_range(a.year(), a.month()),
                    _ => year_range(a.year()),
                };
                set(&mut out, &mut used, r, format!("{k} {next} ago"), i..i + 3);
            }
            i += 3;
            continue;
        }

        // ---- ISO-ish dates --------------------------------------------
        if let Some(r) = iso_like(t) {
            let label = t.to_string();
            if set(&mut out, &mut used, Some(r.0), label, i..i + 1) {
                date_terms.extend(r.1);
            }
            i += 1;
            continue;
        }
        if let Some(y) = year4(t) {
            if y <= today.year() + 1 && set(&mut out, &mut used, year_range(y), y.to_string(), i..i + 1) {
                date_terms.push(y.to_string());
            }
            i += 1;
            continue;
        }
        i += 1;
    }

    // ---- folder references ("from downloads", "in my zed folder") -----
    if find_places {
        for i in 0..n {
            if used[i] != Used::No || !PREPOSITIONS.contains(&low(i)) {
                continue;
            }
            let mut k = i + 1;
            while k < n && used[k] == Used::No && POSSESSIVES.contains(&low(k)) {
                k += 1;
            }
            let mut words = Vec::new();
            while k < n && words.len() < 3 && used[k] == Used::No {
                let t = low(k);
                if PREPOSITIONS.contains(&t) || FOLDER_WORDS.contains(&t) || STOP.contains(&t) {
                    break;
                }
                words.push(k);
                k += 1;
            }
            let trailing_folder = k < n && used[k] == Used::No && FOLDER_WORDS.contains(&low(k));
            for len in (1..=words.len()).rev() {
                let mut span: Vec<usize> = (i..=words[len - 1]).collect();
                if len == words.len() && trailing_folder {
                    span.push(k);
                }
                let phrase = words[..len].iter().map(|&w| toks[w].orig.as_str()).collect::<Vec<_>>().join(" ");
                let without = parse_inner(query, now, &span, false);
                out.places.push(Place { phrase, without: Box::new(without) });
            }
        }
    }

    // ---- kinds and scope ----------------------------------------------
    for i in 0..n {
        if used[i] != Used::No {
            continue;
        }
        let t = low(i);
        if let Some(ks) = kind_of(t) {
            for k in ks {
                if !out.kinds.contains(k) {
                    out.kinds.push(*k);
                }
            }
            used[i] = Used::Kind;
            continue;
        }
        let next = low(i + 1);
        let is_scope = t == "here"
            || ((t == "this" || t == "current") && matches!(next, "folder" | "directory" | "dir" | "project" | "repo"));
        if is_scope {
            out.scope_here = true;
            used[i] = Used::Scope;
            if t != "here" {
                used[i + 1] = Used::Scope;
            }
        }
    }

    let mut semantic = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if used[i] != Used::No || STOP.contains(&tok.low.as_str()) {
            continue;
        }
        semantic.push(tok.orig.clone());
        let term: String = tok.low.chars().filter(|c| c.is_alphanumeric()).collect();
        if term.len() >= 2 && !out.terms.contains(&term) {
            out.terms.push(term);
        }
    }
    out.semantic = semantic.join(" ");
    for d in date_terms {
        if !out.terms.contains(&d) {
            out.terms.push(d);
        }
    }
    out
}

fn iso_like(t: &str) -> Option<(CivilRange, Vec<String>)> {
    let parts: Vec<&str> = t.split(['-', '/']).collect();
    match parts.as_slice() {
        [y, m] if y.len() == 4 => {
            let (y, m) = (year4(y)?, m.parse::<i8>().ok()?);
            Some((month_range(y, m)?, vec![y.to_string()]))
        }
        [m, y] if y.len() == 4 && m.len() <= 2 => {
            let (y, m) = (year4(y)?, m.parse::<i8>().ok()?);
            Some((month_range(y, m)?, vec![y.to_string()]))
        }
        [y, m, d] if y.len() == 4 => {
            let d = Date::new(year4(y)?, m.parse().ok()?, d.parse().ok()?).ok()?;
            Some((CivilRange::days(d, d.checked_add(1.day()).ok()?), vec![d.year().to_string()]))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::civil_to_string;

    fn now() -> Zoned {
        "2026-10-18T16:00:00[UTC]".parse().unwrap()
    }

    fn range(q: &ParsedQuery) -> (String, String) {
        let r = q.range.expect("range");
        (civil_to_string(r.start), civil_to_string(r.end))
    }

    #[test]
    fn photos_december_last_year() {
        let q = parse("all photos from december last year", &now());
        assert_eq!(q.kinds, vec![Category::Image]);
        assert_eq!(range(&q), ("2025-12-01T00:00:00".into(), "2026-01-01T00:00:00".into()));
        assert_eq!(q.semantic, "");
    }

    #[test]
    fn photos_of_mountains_last_december() {
        let q = parse("photos of mountains from last December", &now());
        assert_eq!(q.kinds, vec![Category::Image]);
        assert_eq!(range(&q).0, "2025-12-01T00:00:00");
        assert_eq!(q.semantic, "mountains");
    }

    #[test]
    fn sales_report_25_last_quarter() {
        let q = parse("sales report for 25's last quarter", &now());
        assert_eq!(range(&q), ("2025-10-01T00:00:00".into(), "2026-01-01T00:00:00".into()));
        assert_eq!(q.semantic, "sales report");
        assert!(q.terms.contains(&"q4".to_string()) && q.terms.contains(&"2025".to_string()));
    }

    #[test]
    fn quarter_forms() {
        for s in ["Q4 2025 revenue", "q4'25 revenue", "q4-2025 revenue", "fourth quarter of 2025 revenue", "last quarter of 2025 revenue"] {
            let q = parse(s, &now());
            assert_eq!(range(&q).0, "2025-10-01T00:00:00", "{s}");
            assert_eq!(q.semantic, "revenue", "{s}");
        }
        let q = parse("last quarter invoices", &now());
        assert_eq!(range(&q).0, "2026-07-01T00:00:00");
    }

    #[test]
    fn sop_this_project() {
        let q = parse("that SOP document clients gave for this project", &now());
        assert!(q.scope_here);
        assert!(q.kinds.contains(&Category::Document));
        assert_eq!(q.semantic, "SOP clients gave");
        assert!(q.range.is_none());
    }

    #[test]
    fn code_query() {
        let q = parse("script that runs terraform on aws", &now());
        assert_eq!(q.kinds, vec![Category::Code]);
        assert_eq!(q.semantic, "runs terraform aws");
    }

    #[test]
    fn may_is_not_always_a_month() {
        let q = parse("files that may contain invoices", &now());
        assert!(q.range.is_none());
        let q = parse("photos may 2024", &now());
        assert_eq!(range(&q).0, "2024-05-01T00:00:00");
    }

    #[test]
    fn bare_month_is_most_recent_past() {
        let q = parse("november trip", &now());
        assert_eq!(range(&q).0, "2025-11-01T00:00:00");
        let q = parse("october trip", &now());
        assert_eq!(range(&q).0, "2026-10-01T00:00:00");
    }

    #[test]
    fn relative_units() {
        let q = parse("screenshots last week", &now());
        // 2026-10-18 is a Sunday; this week starts Monday 2026-10-12
        assert_eq!(range(&q), ("2026-10-05T00:00:00".into(), "2026-10-12T00:00:00".into()));
        let q = parse("notes yesterday", &now());
        assert_eq!(range(&q).0, "2026-10-17T00:00:00");
        let q = parse("pdfs past 10 days", &now());
        assert_eq!(range(&q), ("2026-10-08T00:00:00".into(), "2026-10-19T00:00:00".into()));
        let q = parse("photos 2 months ago", &now());
        assert_eq!(range(&q).0, "2026-08-01T00:00:00");
        let q = parse("budget last year", &now());
        assert_eq!(range(&q).0, "2025-01-01T00:00:00");
    }

    #[test]
    fn iso_and_years() {
        let q = parse("invoice 2025-12", &now());
        assert_eq!(range(&q).0, "2025-12-01T00:00:00");
        let q = parse("trip 2024", &now());
        assert_eq!(range(&q).0, "2024-01-01T00:00:00");
        let q = parse("port 8080 config", &now());
        assert!(q.range.is_none());
        let q = parse("2048 game", &now());
        assert!(q.range.is_none());
    }

    #[test]
    fn leap_year_and_month_end() {
        let n: Zoned = "2028-03-31T10:00:00[UTC]".parse().unwrap();
        let q = parse("last month", &n);
        assert_eq!(range(&q), ("2028-02-01T00:00:00".into(), "2028-03-01T00:00:00".into()));
        let n: Zoned = "2026-01-05T10:00:00[UTC]".parse().unwrap();
        let q = parse("last quarter", &n);
        assert_eq!(range(&q).0, "2025-10-01T00:00:00");
    }

    #[test]
    fn folder_references() {
        let q = parse("fire photos from downloads", &now());
        assert_eq!(q.kinds, vec![Category::Image]);
        assert_eq!(q.places.len(), 1);
        assert_eq!(q.places[0].phrase, "downloads");
        assert_eq!(q.places[0].without.semantic, "fire");
        assert_eq!(q.places[0].without.kinds, vec![Category::Image]);
        assert_eq!(q.semantic, "fire downloads"); // if no folder matches

        let q = parse("invoices in my zed folder from december", &now());
        assert_eq!(q.places[0].phrase, "zed");
        assert_eq!(q.places[0].without.semantic, "invoices");
        assert!(q.places[0].without.range.is_some());

        let q = parse("notes from manali trek dec 25", &now());
        assert_eq!(q.places.iter().map(|p| p.phrase.as_str()).collect::<Vec<_>>(), vec!["manali trek", "manali"]);

        // a date after "from" is a date, not a place
        let q = parse("photos from december last year", &now());
        assert!(q.places.is_empty());
    }

    #[test]
    fn garbage_does_not_panic() {
        for s in ["", "   ", "'", "q", "q9", "last", "last 99999999999 days", "\u{0}\u{1}", "may", "of of of", "q4'", "12/", "-/-"] {
            let _ = parse(s, &now());
        }
    }
}
