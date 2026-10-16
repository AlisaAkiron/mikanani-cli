//! `mikan search`: search Mikan, pick a show and a subtitle group, then hand
//! the resulting feed to the shared export flow.

use anyhow::Result;
use inquire::Text;
use std::fmt;

use crate::commands::wizard::{ExportOutcome, run_export_flow};
use crate::mikan;
use crate::net::{self, add_proxy_hint};
use crate::ui::{Spinner, cancelled, required, take_or_cancel};

/// Build a Mikan RSS feed URL. `subgroup_id == 0` means "all groups" and omits
/// the `subgroupid` parameter.
fn rss_url(bangumi_id: u32, subgroup_id: u32) -> String {
    let base = format!("https://mikanani.me/RSS/Bangumi?bangumiId={bangumi_id}");
    if subgroup_id == 0 {
        base
    } else {
        format!("{base}&subgroupid={subgroup_id}")
    }
}

/// The subgroup step's decision: either use an id directly (no prompt) or show
/// a menu. `Auto(0)` means "all groups" (RSS without `subgroupid`).
enum SubgroupChoice {
    Auto(u32),
    Menu(Vec<mikan::Subgroup>),
}

/// Decide the subgroup step from the scraped groups: none → all-groups; one →
/// use it; many → a menu with an "all groups" entry prepended.
fn subgroup_choice(groups: Vec<mikan::Subgroup>) -> SubgroupChoice {
    match groups.len() {
        0 => SubgroupChoice::Auto(0),
        1 => SubgroupChoice::Auto(groups[0].id),
        _ => {
            let mut opts = vec![mikan::Subgroup { name: "全部字幕组".to_string(), id: 0 }];
            opts.extend(groups);
            SubgroupChoice::Menu(opts)
        }
    }
}

/// A choice in the show picker: a scraped show, or "search again" — re-enter
/// the query. `SearchAgain` sits at the bottom of the list so a wrong first
/// hit doesn't force aborting the whole run (Mikan returns at most ~4 hits).
#[derive(Clone)]
enum ShowChoice {
    Pick(mikan::Show),
    SearchAgain,
}

impl fmt::Display for ShowChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShowChoice::Pick(show) => f.write_str(&show.title),
            ShowChoice::SearchAgain => f.write_str("↻ 重新搜索"),
        }
    }
}

/// A choice in the subtitle-group picker: a group (including the synthetic
/// "all groups" entry), or "back" — return to the show list without
/// re-searching.
enum GroupNav {
    Group(mikan::Subgroup),
    Back,
}

impl fmt::Display for GroupNav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GroupNav::Group(group) => f.write_str(&group.name),
            GroupNav::Back => f.write_str("← 返回上一步"),
        }
    }
}

/// Prompt for a Mikan search term. `initial` pre-fills the input, so a
/// re-search can edit the previous term instead of retyping. Ok(None) means
/// the user cancelled; the returned string is trimmed and non-empty.
fn prompt_query(initial: Option<&str>) -> Result<Option<String>> {
    let mut input = Text::new("搜索蜜柑：")
        .with_validator(required("请输入搜索关键词"))
        .with_help_message("回车：搜索 · Esc：取消");
    if let Some(init) = initial {
        input = input.with_initial_value(init);
    }
    Ok(take_or_cancel(input.prompt_skippable(), "搜索关键词")?.map(|q| q.trim().to_string()))
}

/// Show the show picker. Ok(None) on cancel.
fn pick_show(options: &[ShowChoice]) -> Result<Option<ShowChoice>> {
    take_or_cancel(
        inquire::Select::new("番剧：", options.to_vec())
            .with_help_message("输入以筛选 · 回车：选择 · Esc：取消")
            .prompt_skippable(),
        "番剧",
    )
}

/// Show the subtitle-group picker with a trailing "back" entry. Ok(None) on
/// cancel.
fn pick_group(options: &[mikan::Subgroup]) -> Result<Option<GroupNav>> {
    let mut nav: Vec<GroupNav> = options.iter().cloned().map(GroupNav::Group).collect();
    nav.push(GroupNav::Back);
    take_or_cancel(
        inquire::Select::new("字幕组：", nav)
            .with_help_message("输入以筛选 · 回车：选择 · Esc：取消")
            .prompt_skippable(),
        "字幕组",
    )
}

pub fn search_flow(query: Option<String>, proxy: Option<&str>) -> Result<i32> {
    let client = net::build_client(proxy)?;

    // Resolve the initial query (prompt when absent or blank).
    let mut query = match query {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => match prompt_query(None)? {
            Some(q) => q,
            None => return Ok(cancelled()),
        },
    };

    // Mikan returns at most ~4 hits, so the wanted show is often missing from
    // the first search. This loop lets the user edit the term and search again
    // from the show list, or back out of a show from the subgroup list —
    // without aborting and re-running the CLI. Esc still cancels everywhere.
    'search: loop {
        let shows = {
            let _spinner = Spinner::start("正在搜索…");
            mikan::search_shows(&client, &query).map_err(add_proxy_hint)?
        };
        // No results is not a dead end: re-prompt (pre-filled) to refine.
        if shows.is_empty() {
            println!("未找到与「{query}」匹配的番剧");
            let Some(q) = prompt_query(Some(&query))? else {
                return Ok(cancelled());
            };
            query = q;
            continue 'search;
        }

        // The scraped shows, plus a trailing "search again" entry.
        let mut show_options: Vec<ShowChoice> = shows.into_iter().map(ShowChoice::Pick).collect();
        show_options.push(ShowChoice::SearchAgain);

        // Pick a show. "Back" from the subgroup step returns here without
        // re-searching.
        'show: loop {
            let show = match pick_show(&show_options)? {
                None => return Ok(cancelled()),
                Some(ShowChoice::Pick(show)) => show,
                Some(ShowChoice::SearchAgain) => {
                    let Some(q) = prompt_query(Some(&query))? else {
                        return Ok(cancelled());
                    };
                    query = q;
                    continue 'search;
                }
            };

            // Fetch subtitle groups once per show; the 'group loop below
            // re-shows the menu (without re-scraping) when a later step
            // navigates back.
            let groups = {
                let _spinner = Spinner::start("正在加载字幕组…");
                mikan::subgroups(&client, show.id).map_err(add_proxy_hint)?
            };
            let choice = subgroup_choice(groups);
            // Where "back" lands: the group menu when one exists, else the
            // show list (the group step was auto-skipped).
            let has_menu = matches!(choice, SubgroupChoice::Menu(_));
            'group: loop {
                let subgroup_id = match &choice {
                    SubgroupChoice::Auto(id) => *id,
                    SubgroupChoice::Menu(options) => match pick_group(options)? {
                        None => return Ok(cancelled()),
                        Some(GroupNav::Group(group)) => group.id,
                        Some(GroupNav::Back) => continue 'show,
                    },
                };

                // Build the RSS URL and reuse the shared export flow.
                let feed = net::fetch_feed(&client, &rss_url(show.id, subgroup_id))?;
                // A dud choice (empty feed) navigates back instead of
                // aborting, like Esc from the episode picker.
                if feed.episodes.is_empty() {
                    println!("订阅「{}」中没有剧集", feed.channel_title);
                } else {
                    match run_export_flow(&client, feed, true)? {
                        ExportOutcome::Exit(code) => return Ok(code),
                        ExportOutcome::Back => {}
                    }
                }
                if has_menu {
                    continue 'group;
                }
                continue 'show;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_choice_display() {
        let pick = ShowChoice::Pick(mikan::Show { title: "石纪元".to_string(), id: 3689 });
        assert_eq!(pick.to_string(), "石纪元");
        assert_eq!(ShowChoice::SearchAgain.to_string(), "↻ 重新搜索");
    }

    #[test]
    fn group_nav_display() {
        let group = GroupNav::Group(mikan::Subgroup { name: "猎户发布组".to_string(), id: 597 });
        assert_eq!(group.to_string(), "猎户发布组");
        assert_eq!(GroupNav::Back.to_string(), "← 返回上一步");
    }

    #[test]
    fn rss_url_omits_subgroup_when_zero() {
        assert_eq!(rss_url(3950, 0), "https://mikanani.me/RSS/Bangumi?bangumiId=3950");
        assert_eq!(
            rss_url(3950, 597),
            "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
        );
    }

    #[test]
    fn subgroup_choice_auto_and_menu() {
        // No scraped groups → all-groups, no prompt.
        assert!(matches!(subgroup_choice(vec![]), SubgroupChoice::Auto(0)));

        // Exactly one → use it, no prompt.
        let one = vec![mikan::Subgroup { name: "A".into(), id: 597 }];
        assert!(matches!(subgroup_choice(one), SubgroupChoice::Auto(597)));

        // Two or more → menu with "all groups" (id 0) prepended.
        let two = vec![
            mikan::Subgroup { name: "A".into(), id: 597 },
            mikan::Subgroup { name: "B".into(), id: 611 },
        ];
        match subgroup_choice(two) {
            SubgroupChoice::Menu(opts) => {
                assert_eq!(opts.len(), 3);
                assert_eq!(opts[0].id, 0);
                assert!(opts[0].name.contains("全部字幕组"));
                assert_eq!(opts[1].id, 597);
                assert_eq!(opts[2].id, 611);
            }
            SubgroupChoice::Auto(_) => panic!("expected a menu for two groups"),
        }
    }
}
