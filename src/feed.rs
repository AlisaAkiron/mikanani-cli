use anyhow::Context;
use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Default, Clone)]
pub struct Episode {
    pub title: String,
    pub torrent_url: String,
    pub size: Option<u64>,
    pub pub_date: Option<String>,
}

#[derive(Debug, Default)]
pub struct Feed {
    pub channel_title: String,
    pub episodes: Vec<Episode>,
}

pub fn fetch_feed(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<Feed> {
    let xml = client
        .get(url)
        .send()
        .context("请求 RSS 订阅")?
        .error_for_status()
        .context("订阅请求被拒绝")?
        .text()
        .context("读取订阅内容")?;
    parse_feed(&xml)
}

pub fn parse_feed(xml: &str) -> anyhow::Result<Feed> {
    let mut reader = Reader::from_str(xml);

    let mut feed = Feed::default();
    let mut path: Vec<String> = Vec::new();
    let mut current: Option<Episode> = None;

    loop {
        match reader.read_event().context("解析订阅 XML")? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "item" {
                    current = Some(Episode::default());
                }
                path.push(name);
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == b"enclosure"
                    && let Some(ep) = current.as_mut()
                {
                    for attr in e.attributes().flatten() {
                        // Same hostile-feed hygiene as append_text: these
                        // values reach the terminal and exported files.
                        let raw = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .unwrap_or_default();
                        let value = crate::sanitize::strip_unsafe(&raw);
                        match attr.key.local_name().as_ref() {
                            b"url" => ep.torrent_url = value,
                            b"length" => ep.size = value.parse().ok(),
                            _ => {}
                        }
                    }
                }
            }
            Event::Text(t) => {
                let text = t.xml10_content().unwrap_or_default().into_owned();
                append_text(&path, &mut feed, &mut current, &text);
            }
            Event::CData(t) => {
                let text = String::from_utf8_lossy(&t.into_inner()).into_owned();
                append_text(&path, &mut feed, &mut current, &text);
            }
            Event::GeneralRef(r) => {
                // Entity/character references arrive as separate events; resolve
                // the common ones so titles like "A &amp; B" survive intact.
                let resolved = match r.resolve_char_ref().ok().flatten() {
                    Some(ch) => ch.to_string(),
                    None => {
                        let entity: &[u8] = r.as_ref();
                        match entity {
                            x if x == b"amp" => "&".to_string(),
                            x if x == b"lt" => "<".to_string(),
                            x if x == b"gt" => ">".to_string(),
                            x if x == b"apos" => "'".to_string(),
                            x if x == b"quot" => "\"".to_string(),
                            _ => String::new(),
                        }
                    }
                };
                append_text(&path, &mut feed, &mut current, &resolved);
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"item"
                    && let Some(mut ep) = current.take()
                {
                    ep.title = ep.title.trim().to_string();
                    ep.pub_date = ep
                        .pub_date
                        .map(|d| d.trim().to_string())
                        .filter(|d| !d.is_empty());
                    if !ep.title.is_empty() && !ep.torrent_url.is_empty() {
                        feed.episodes.push(ep);
                    }
                }
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    feed.channel_title = feed.channel_title.trim().to_string();
    Ok(feed)
}

fn append_text(path: &[String], feed: &mut Feed, current: &mut Option<Episode>, text: &str) {
    // These strings are remote-controlled and printed to the terminal: strip
    // control chars (ESC, BEL, CR/LF, …) plus invisible/bidi format chars
    // before they reach any output.
    let clean = crate::sanitize::strip_unsafe(text);
    let text = clean.as_str();

    match path.last().map(String::as_str) {
        Some("title") if path == ["rss", "channel", "title"] => feed.channel_title.push_str(text),
        Some("title") => {
            if let Some(ep) = current.as_mut() {
                ep.title.push_str(text);
            }
        }
        Some("pubDate") if path.iter().any(|s| s == "torrent") => {
            if let Some(ep) = current.as_mut() {
                ep.pub_date.get_or_insert_with(String::new).push_str(text);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/dr_stone.xml");

    #[test]
    fn parses_real_feed() {
        let feed = parse_feed(FIXTURE).unwrap();
        assert_eq!(feed.channel_title, "Mikan Project - 石纪元 科学与未来 第3部分");
        assert_eq!(feed.episodes.len(), 26);

        let first = &feed.episodes[0];
        assert_eq!(
            first.title,
            "[猎户压制部] 新石纪 第四季 科学与未来 / Dr.STONE：Science Future [37] [1080p] [繁日内嵌] [2026年4月番]"
        );
        assert_eq!(
            first.torrent_url,
            "https://mikanani.me/Download/20260628/b66771fd95710ab32a19a4797994196780c94527.torrent"
        );
        assert_eq!(first.size, Some(875560960));
        assert_eq!(first.pub_date.as_deref(), Some("2026-06-28T23:10:00"));
    }

    #[test]
    fn every_episode_has_url_and_date() {
        let feed = parse_feed(FIXTURE).unwrap();
        assert!(feed.episodes.iter().all(|e| !e.torrent_url.is_empty()));
        assert!(feed.episodes.iter().all(|e| e.pub_date.is_some()));
    }

    #[test]
    fn well_formed_non_feed_xml_yields_zero_episodes() {
        let feed = parse_feed("<rss><channel><title>t</title></channel></rss>").unwrap();
        assert_eq!(feed.channel_title, "t");
        assert!(feed.episodes.is_empty());
    }

    #[test]
    fn ill_formed_xml_is_an_error() {
        assert!(parse_feed("<rss><channel></chanel></rss>").is_err());
    }

    #[test]
    fn item_without_enclosure_is_dropped() {
        let xml = r#"<rss><channel><title>t</title>
            <item><title>no enclosure</title></item>
            <item><title>good</title><enclosure url="http://x/a.torrent" length="10"/></item>
        </channel></rss>"#;
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].title, "good");
    }

    #[test]
    fn resolves_entity_and_char_refs_in_titles() {
        let xml = r#"<rss><channel><title>A &amp; B</title>
            <item><title>Fate &amp; Prisma &#38; Illya</title><enclosure url="http://x/a.torrent" length="10"/></item>
        </channel></rss>"#;
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.channel_title, "A & B");
        assert_eq!(feed.episodes[0].title, "Fate & Prisma & Illya");
    }

    #[test]
    fn strips_control_chars_from_remote_strings() {
        let xml = "<rss><channel><title>A\u{1b}]0;pwned\u{7}B</title>\
            <item><title>bad\rtitle</title><enclosure url=\"http://x/a.torrent\" length=\"1\"/></item>\
            </channel></rss>";
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.channel_title, "A]0;pwnedB");
        assert_eq!(feed.episodes[0].title, "badtitle");
    }

    #[test]
    fn strips_bidi_and_zero_width_from_remote_strings() {
        // A RIGHT-TO-LEFT OVERRIDE could reorder the displayed title in the
        // picker; a zero-width space could hide a boundary. Neither survives.
        let xml = "<rss><channel><title>ok</title>\
            <item><title>good\u{202e}evil\u{200b}!</title>\
            <enclosure url=\"http://x/a.torrent\" length=\"1\"/></item>\
            </channel></rss>";
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.episodes[0].title, "goodevil!");
    }

    #[test]
    fn parses_cdata_titles() {
        let xml = r#"<rss><channel><title><![CDATA[A & B]]></title>
            <item><title><![CDATA[Ep [01] <final>]]></title><enclosure url="http://x/a.torrent" length="1"/></item>
        </channel></rss>"#;
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.channel_title, "A & B");
        assert_eq!(feed.episodes[0].title, "Ep [01] <final>");
    }

    #[test]
    fn item_without_title_is_dropped() {
        let xml = r#"<rss><channel><title>t</title>
            <item><enclosure url="http://x/a.torrent" length="1"/></item>
            <item><title>good</title><enclosure url="http://x/b.torrent" length="1"/></item>
        </channel></rss>"#;
        let feed = parse_feed(xml).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].title, "good");
    }

    #[test]
    fn strips_control_chars_from_enclosure_urls() {
        let xml = r#"<rss><channel><title>t</title>
            <item><title>ep</title><enclosure url="http://x/a.torrent&#10;http://evil/b.torrent" length="1"/></item>
        </channel></rss>"#;
        let feed = parse_feed(xml).unwrap();
        assert_eq!(
            feed.episodes[0].torrent_url,
            "http://x/a.torrenthttp://evil/b.torrent"
        );
    }
}
