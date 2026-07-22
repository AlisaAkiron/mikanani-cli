use crate::feed::Episode;

/// Sort newest-first, but only when every episode is dated — otherwise trust
/// the feed's own (already newest-first) order. ISO 8601 date strings compare
/// chronologically.
pub(crate) fn sort_episodes(mut episodes: Vec<Episode>) -> Vec<Episode> {
    if episodes.iter().all(|e| e.pub_date.is_some()) {
        episodes.sort_by(|a, b| b.pub_date.cmp(&a.pub_date));
    }
    episodes
}

/// Non-interactive episode selection: keep titles containing `filter`
/// (case-insensitive substring) when given, sort newest-first, then keep the
/// first `latest` when given. With neither, the whole (sorted) feed is
/// returned — the caller's `--all`.
pub(crate) fn select(
    episodes: Vec<Episode>,
    latest: Option<usize>,
    filter: Option<&str>,
) -> Vec<Episode> {
    let kept = match filter {
        Some(needle) => {
            let needle = needle.to_lowercase();
            episodes
                .into_iter()
                .filter(|e| e.title.to_lowercase().contains(&needle))
                .collect()
        }
        None => episodes,
    };
    let mut sorted = sort_episodes(kept);
    if let Some(n) = latest {
        sorted.truncate(n);
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(title: &str, date: Option<&str>) -> Episode {
        Episode {
            title: title.to_string(),
            torrent_url: "http://x/a.torrent".to_string(),
            size: None,
            pub_date: date.map(str::to_string),
        }
    }

    fn titles(episodes: &[Episode]) -> Vec<&str> {
        episodes.iter().map(|e| e.title.as_str()).collect()
    }

    #[test]
    fn sorts_newest_first_when_all_dated() {
        let sorted = sort_episodes(vec![
            ep("old", Some("2026-06-21T22:24:00")),
            ep("new", Some("2026-06-28T23:10:00")),
        ]);
        assert_eq!(sorted[0].title, "new");
    }

    #[test]
    fn keeps_feed_order_when_any_date_missing() {
        let sorted = sort_episodes(vec![
            ep("first", Some("2026-06-21T22:24:00")),
            ep("second", None),
            ep("third", Some("2026-06-28T23:10:00")),
        ]);
        assert_eq!(titles(&sorted), ["first", "second", "third"]);
    }

    #[test]
    fn all_returns_everything_sorted_newest_first() {
        let out = select(
            vec![
                ep("a", Some("2026-06-21T00:00:00")),
                ep("b", Some("2026-06-28T00:00:00")),
            ],
            None,
            None,
        );
        assert_eq!(titles(&out), ["b", "a"]);
    }

    #[test]
    fn latest_keeps_newest_n() {
        let out = select(
            vec![
                ep("a", Some("2026-06-21T00:00:00")),
                ep("b", Some("2026-06-28T00:00:00")),
                ep("c", Some("2026-06-25T00:00:00")),
            ],
            Some(2),
            None,
        );
        assert_eq!(titles(&out), ["b", "c"]);
    }

    #[test]
    fn latest_larger_than_feed_returns_all() {
        let out = select(vec![ep("only", Some("2026-06-21T00:00:00"))], Some(5), None);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let out = select(
            vec![
                ep("[Group] Show 1080p [01]", None),
                ep("[Group] Show 720p [02]", None),
            ],
            None,
            Some("1080P"),
        );
        assert_eq!(titles(&out), ["[Group] Show 1080p [01]"]);
    }

    #[test]
    fn filter_matches_cjk_substring() {
        let out = select(
            vec![ep("[组] 石纪元 [37] [繁日]", None), ep("[组] 石纪元 [37] [简日]", None)],
            None,
            Some("繁日"),
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].title.contains("繁日"));
    }

    #[test]
    fn filter_no_match_is_empty() {
        let out = select(vec![ep("Show 720p", None)], None, Some("2160p"));
        assert!(out.is_empty());
    }

    #[test]
    fn filter_then_latest_compose() {
        // 1080p titles: A(21), C(25), D(20) → newest-first C, A, D → latest 2 → C, A
        let out = select(
            vec![
                ep("A 1080p", Some("2026-06-21T00:00:00")),
                ep("B 720p", Some("2026-06-28T00:00:00")),
                ep("C 1080p", Some("2026-06-25T00:00:00")),
                ep("D 1080p", Some("2026-06-20T00:00:00")),
            ],
            Some(2),
            Some("1080p"),
        );
        assert_eq!(titles(&out), ["C 1080p", "A 1080p"]);
    }
}
