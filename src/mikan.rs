use anyhow::{Context, Result};
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::fmt;

/// A Mikan bangumi (show) as surfaced by search.
#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    pub title: String,
    pub id: u32,
}

impl fmt::Display for Show {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.title)
    }
}

/// A subtitle group publishing a given bangumi. `id == 0` is reserved by the
/// caller for a synthetic "all groups" entry (RSS without `subgroupid`).
#[derive(Debug, Clone, PartialEq)]
pub struct Subgroup {
    pub name: String,
    pub id: u32,
}

impl fmt::Display for Subgroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// Extract distinct shows from a `/Home/Search` results page. Each result
/// card is an `a[href^="/Home/Bangumi/"]`; its title lives in a descendant
/// `div.an-text` (prefer that div's `title` attribute, falling back to its
/// text). Cards without a `div.an-text` fall back to the anchor's own
/// `title` attribute, then its text. Skips anchors whose id isn't numeric or
/// whose name is empty; dedups by bangumi id (the page links each show more
/// than once — poster + title).
fn parse_shows(html: &str) -> Vec<Show> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"a[href^="/Home/Bangumi/"]"#).expect("valid selector");
    let an_text = Selector::parse("div.an-text").expect("valid selector");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for a in doc.select(&sel) {
        let href = a.value().attr("href").unwrap_or_default();
        let Some(id) = href
            .strip_prefix("/Home/Bangumi/")
            .map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let raw = a
            .select(&an_text)
            .next()
            .map(|div| {
                div.value()
                    .attr("title")
                    .map(str::to_string)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| div.text().collect::<String>())
            })
            .unwrap_or_else(|| {
                a.value()
                    .attr("title")
                    .map(str::to_string)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| a.text().collect::<String>())
            });
        let title = crate::sanitize::strip_unsafe(&raw).trim().to_string();
        if title.is_empty() {
            continue;
        }
        if seen.insert(id) {
            out.push(Show { title, id });
        }
    }
    out
}

/// Extract subtitle groups from a `/Home/Bangumi/<id>` page. Each
/// `div.subgroup-text` block carries a `/RSS/Bangumi?...&subgroupid=<sid>`
/// link (the feed) and, usually, a `/Home/PublishGroup/<gid>` link (the group
/// name). Some blocks have no `PublishGroup` anchor at all — the name is
/// instead the div's leading text node(s) (e.g. "生肉/不明字幕"), which must
/// NOT be confused with `div.text()`: that absorbs trailing/hidden markup
/// such as the `已订阅` span. Blocks without an RSS link are skipped; groups
/// are deduped by subgroup id.
fn parse_subgroups(html: &str) -> Vec<Subgroup> {
    let doc = Html::parse_document(html);
    let block = Selector::parse("div.subgroup-text").expect("valid selector");
    let rss = Selector::parse(r#"a[href*="/RSS/Bangumi"]"#).expect("valid selector");
    let group = Selector::parse(r#"a[href^="/Home/PublishGroup/"]"#).expect("valid selector");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for div in doc.select(&block) {
        let Some(id) = div
            .select(&rss)
            .next()
            .and_then(|a| a.value().attr("href"))
            .and_then(subgroupid_from)
        else {
            continue;
        };
        let raw = div
            .select(&group)
            .next()
            .map(|a| a.text().collect::<String>())
            .unwrap_or_else(|| leading_text(&div));
        let name = crate::sanitize::strip_unsafe(&raw).trim().to_string();
        if name.is_empty() {
            continue;
        }
        if seen.insert(id) {
            out.push(Subgroup { name, id });
        }
    }
    out
}

/// Concatenate a div's leading text node(s) — the text before its first
/// child element — trimmed. Used as the subgroup-name fallback so hidden
/// sibling markup (e.g. a `已订阅` span) that comes after the first child
/// element is never absorbed the way `div.text()` would absorb it.
fn leading_text(div: &scraper::ElementRef) -> String {
    let mut s = String::new();
    for child in div.children() {
        match child.value().as_text() {
            Some(text) => s.push_str(text),
            None => break,
        }
    }
    s
}

/// Pull the numeric `subgroupid` out of an `/RSS/Bangumi?...` query string.
fn subgroupid_from(href: &str) -> Option<u32> {
    href.split("subgroupid=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .and_then(|s| s.parse::<u32>().ok())
}

const BASE: &str = "https://mikanani.me";

/// Search Mikan for shows matching `query`. Network + parse; empty result is
/// `Ok(vec![])`, not an error.
pub fn search_shows(client: &reqwest::blocking::Client, query: &str) -> Result<Vec<Show>> {
    let html = client
        .get(format!("{BASE}/Home/Search"))
        .query(&[("searchstr", query)])
        .send()
        .context("requesting Mikan search")?
        .error_for_status()
        .context("Mikan search request rejected")?
        .text()
        .context("reading Mikan search results")?;
    Ok(parse_shows(&html))
}

/// Fetch the subtitle groups publishing `bangumi_id`. Network + parse; empty
/// result is `Ok(vec![])`, not an error.
pub fn subgroups(client: &reqwest::blocking::Client, bangumi_id: u32) -> Result<Vec<Subgroup>> {
    let html = client
        .get(format!("{BASE}/Home/Bangumi/{bangumi_id}"))
        .send()
        .context("requesting the Mikan bangumi page")?
        .error_for_status()
        .context("Mikan bangumi request rejected")?
        .text()
        .context("reading the Mikan bangumi page")?;
    Ok(parse_subgroups(&html))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = include_str!("../tests/fixtures/mikan_search.html");
    const BANGUMI: &str = include_str!("../tests/fixtures/mikan_bangumi.html");

    #[test]
    fn parse_shows_extracts_and_dedups() {
        let shows = parse_shows(SEARCH);
        assert_eq!(
            shows,
            vec![
                Show { title: "石纪元 科学与未来 第2部分".to_string(), id: 3689 },
                Show { title: "新石纪 NEW WORLD".to_string(), id: 3000 },
                Show { title: "Suffixed Show".to_string(), id: 3952 },
            ]
        );
    }

    #[test]
    fn parse_shows_empty_page_is_empty() {
        assert!(parse_shows("<html><body></body></html>").is_empty());
    }

    #[test]
    fn parse_subgroups_extracts_name_and_id() {
        let groups = parse_subgroups(BANGUMI);
        assert_eq!(
            groups,
            vec![
                Subgroup { name: "猎户发布组".to_string(), id: 597 },
                Subgroup { name: "生肉/不明字幕".to_string(), id: 202 },
            ]
        );
    }

    #[test]
    fn parse_subgroups_empty_page_is_empty() {
        assert!(parse_subgroups("<html><body></body></html>").is_empty());
    }
}
