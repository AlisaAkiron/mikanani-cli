//! The interactive export flow: pick episodes → outputs → path →
//! qBittorrent → execute. Entered directly with a feed URL, or from the
//! search flow.

use anyhow::{Context, Result, bail};
use inquire::autocompletion::Replacement;
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::{Autocomplete, CustomUserError, InquireError, MultiSelect, Text};
use std::fmt;
use std::path::PathBuf;

use crate::mikan::feed::{self, Episode};
use crate::mikan::select;
use crate::output::{batch, url_list};
use crate::qbt::profile;
use crate::ui::{cancelled, required, take_or_cancel};
use crate::{config, net};

/// `mikan <URL>`: fetch the feed and run the export flow over it.
pub fn wizard(url: &str, proxy: Option<&str>) -> Result<i32> {
    let client = net::build_client(proxy)?;
    let feed = net::fetch_feed(&client, url)?;
    if feed.episodes.is_empty() {
        bail!("订阅「{}」中没有剧集", feed.channel_title);
    }
    match run_export_flow(&client, feed, false)? {
        ExportOutcome::Exit(code) => Ok(code),
        // can_go_back is false above, so the picker never yields Back.
        ExportOutcome::Back => Ok(0),
    }
}

/// How the export flow ended: a normal process exit (with its code), or a
/// request to return to the previous wizard step (search flow only).
pub enum ExportOutcome {
    Exit(i32),
    Back,
}

/// Runs the interactive picker → format → path → qBittorrent → execute steps
/// over an already-fetched feed. Shared by the URL wizard and the search flow.
/// `can_go_back` lets the episode picker's Esc return to the caller's
/// previous step (`ExportOutcome::Back`) instead of cancelling — the search
/// flow has a subtitle-group step to go back to; the URL wizard does not.
pub fn run_export_flow(
    client: &reqwest::blocking::Client,
    feed: feed::Feed,
    can_go_back: bool,
) -> Result<ExportOutcome> {
    let feed::Feed { channel_title, episodes } = feed;
    let cancel = || Ok(ExportOutcome::Exit(cancelled()));

    // Step 1: pick episodes.
    let selected: Vec<Episode> = match pick_episodes(&channel_title, episodes, can_go_back)? {
        PickerAction::Picked(rows) => rows.into_iter().map(|row| row.0).collect(),
        PickerAction::NothingTicked => {
            // Enter with nothing ticked — a deliberate "never mind".
            println!("未选择任何内容");
            return Ok(ExportOutcome::Exit(0));
        }
        PickerAction::Back => return Ok(ExportOutcome::Back),
        // Esc (with no step to go back to) or Ctrl-C — the same "cancelled"
        // as every later step.
        PickerAction::Cancel => return cancel(),
    };

    // Step 2: what to export.
    let Some(outputs) = pick_outputs(&channel_title)? else {
        return cancel();
    };

    // Step 3: export path — only when something is written to disk.
    let config_dir = config::config_dir();
    let mut cfg = config::Config::load(&config_dir)?;
    let export_dir = if outputs.torrents || outputs.urls {
        let Some(dir) = prompt_export_dir(&cfg.path_history)? else {
            return cancel();
        };
        Some(dir)
    } else {
        None
    };

    // Step 4: qBt profile + category.
    let qbt_setup = if outputs.qbt {
        let Some(setup) = profile::resolve_qbt(&cfg, &channel_title)? else {
            return cancel();
        };
        Some(setup)
    } else {
        None
    };

    // Execute. Profile persistence first (it's config state, tested by
    // connect), then category, then filesystem, then the per-episode loop.
    if let Some(setup) = &qbt_setup {
        if let Some((name, profile)) = &setup.save_as {
            cfg.qbt.insert(name.clone(), profile.clone());
            if let Err(e) = cfg.save(&config_dir) {
                eprintln!("警告：无法保存 qBittorrent 配置：{e:#}");
            } else {
                println!("配置「{name}」已保存");
            }
        }
        setup.client.ensure_category(&setup.category)?;
    }

    // Downloading .torrent files to disk is gated on `outputs.torrents`;
    // adding to qBittorrent is gated on `qbt_setup`. Hand the loop those two
    // facts.
    let torrents_dir = if outputs.torrents { export_dir.as_deref() } else { None };
    let totals = batch::process_episodes(client, &selected, torrents_dir, qbt_setup.as_ref());
    totals.print_summary(outputs.torrents, outputs.qbt);

    let mut url_list = None;
    let mut url_failed = false;
    if outputs.urls {
        let dir = export_dir.as_ref().expect("path was asked when urls are exported");
        url_list = batch::write_url_list(&selected, dir, &channel_title);
        url_failed = url_list.is_none();
    }

    // Path history: only when the path step actually ran and produced.
    if let Some(dir) = &export_dir {
        let any_disk_success = totals.downloaded > 0 || totals.skipped > 0 || url_list.is_some();
        if any_disk_success {
            cfg.record_path(&dir.to_string_lossy());
            if let Err(e) = cfg.save(&config_dir) {
                eprintln!("警告：无法保存路径历史：{e:#}");
            }
        }
    }

    let any_failure = totals.any_failure() || url_failed;
    Ok(ExportOutcome::Exit(if any_failure { 1 } else { 0 }))
}

// ---------------------------------------------------------------------------
// Step 1: episode picker

struct Row(Episode);

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = self.0.size.map_or_else(|| "?".to_string(), fmt_size);
        let date = match self.0.pub_date.as_deref() {
            Some(d) => d.get(..10).unwrap_or("?"),
            _ => "?",
        };
        write!(f, "{}  ({size}, {date})", self.0.title)
    }
}

fn fmt_size(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else {
        format!("{:.1} MiB", bytes / MIB)
    }
}

/// What the episode picker's outcome means for the flow. Esc means "back"
/// only when there is a previous step to return to (the search flow);
/// Ctrl-C always cancels. Generic over the row type so it is testable
/// without building inquire rows.
enum PickerAction<T> {
    Picked(Vec<T>),
    NothingTicked,
    Back,
    Cancel,
}

fn pick_episodes(
    channel_title: &str,
    episodes: Vec<Episode>,
    can_go_back: bool,
) -> Result<PickerAction<Row>> {
    let rows: Vec<Row> = select::sort_episodes(episodes).into_iter().map(Row).collect();
    let result = MultiSelect::new(channel_title, rows)
        .with_page_size(15)
        .with_formatter(&|selected| episode_selection_summary(selected))
        .with_help_message(episode_picker_help(can_go_back))
        .prompt_skippable();
    classify_pick(result, can_go_back)
}

/// Post-selection summary for the episode picker. Inquire's default
/// formatter comma-joins every chosen row's `Display`, which for long
/// episode titles collapses into an unreadable wrapped blob. Instead
/// render a count + total-size header and a clean vertical list, on its
/// own lines below the prompt (a leading newline pushes it clear of the
/// answered question).
fn episode_selection_summary(selected: &[ListOption<&Row>]) -> String {
    let count = selected.len();
    let mut header = format!("\n{count} 集");
    if selected.iter().any(|opt| opt.value.0.size.is_some()) {
        let total: u64 = selected.iter().filter_map(|opt| opt.value.0.size).sum();
        header.push_str(&format!(" · {}", fmt_size(total)));
    }

    let mut out = header;
    for opt in selected {
        out.push_str("\n  • ");
        out.push_str(&opt.value.0.title);
    }
    out
}

/// Help line for the episode picker; Esc's meaning depends on whether a
/// previous step exists to go back to.
fn episode_picker_help(can_go_back: bool) -> &'static str {
    if can_go_back {
        "输入以筛选 · 空格：选中/取消 · →/←：全选/全不选 · 回车：下一步 · Esc：返回上一步"
    } else {
        "输入以筛选 · 空格：选中/取消 · →/←：全选/全不选 · 回车：下一步 · Esc：取消"
    }
}

fn classify_pick<T>(
    result: Result<Option<Vec<T>>, InquireError>,
    can_go_back: bool,
) -> Result<PickerAction<T>> {
    match result {
        Ok(Some(picked)) if !picked.is_empty() => Ok(PickerAction::Picked(picked)),
        Ok(Some(_)) => Ok(PickerAction::NothingTicked),
        Ok(None) if can_go_back => Ok(PickerAction::Back),
        Ok(None) | Err(InquireError::OperationInterrupted) => Ok(PickerAction::Cancel),
        Err(e) => Err(e).context("显示剧集选择器"),
    }
}

// ---------------------------------------------------------------------------
// Step 2: output formats

#[derive(Clone, Copy, PartialEq)]
enum ExportFormat {
    TorrentFiles,
    UrlList,
    Qbt,
}

struct FormatOption {
    format: ExportFormat,
    label: String,
}

impl fmt::Display for FormatOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// The outputs ticked in step 2.
struct Outputs {
    torrents: bool,
    urls: bool,
    qbt: bool,
}

/// Ask which outputs to produce (at least one). Ok(None) on cancel.
fn pick_outputs(channel_title: &str) -> Result<Option<Outputs>> {
    let options = vec![
        FormatOption {
            format: ExportFormat::TorrentFiles,
            label: "下载 .torrent 文件".to_string(),
        },
        FormatOption {
            format: ExportFormat::UrlList,
            label: format!("将种子 URL 列表写入「{}」", url_list::url_list_filename(channel_title)),
        },
        FormatOption {
            format: ExportFormat::Qbt,
            label: "添加到 qBittorrent".to_string(),
        },
    ];
    let Some(formats) = take_or_cancel(
        MultiSelect::new("导出为：", options)
            .with_default(&[0])
            .with_validator(|opts: &[ListOption<&FormatOption>]| {
                if opts.is_empty() {
                    Ok(Validation::Invalid("至少选择一种格式".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .with_formatter(&|selected| {
                selected
                    .iter()
                    .map(|opt| format!("\n  • {}", opt.value.label))
                    .collect::<String>()
            })
            .with_help_message("空格：选中/取消 · 回车：下一步 · Esc：取消")
            .prompt_skippable(),
        "导出格式",
    )? else {
        return Ok(None);
    };
    let has = |format| formats.iter().any(|o| o.format == format);
    Ok(Some(Outputs {
        torrents: has(ExportFormat::TorrentFiles),
        urls: has(ExportFormat::UrlList),
        qbt: has(ExportFormat::Qbt),
    }))
}

// ---------------------------------------------------------------------------
// Step 3: export path

/// Suggests entries from the persisted path history. Tab fills the
/// highlighted suggestion into the input for further editing; Enter
/// submits whatever the input holds.
#[derive(Clone)]
struct PathAutocomplete {
    history: Vec<String>,
}

impl Autocomplete for PathAutocomplete {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        let needle = input.to_lowercase();
        Ok(self
            .history
            .iter()
            .filter(|p| p.to_lowercase().contains(&needle))
            .cloned()
            .collect())
    }

    fn get_completion(
        &mut self,
        _input: &str,
        highlighted_suggestion: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        Ok(highlighted_suggestion)
    }
}

fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Ask where to export, creating the directory. Ok(None) on cancel.
fn prompt_export_dir(path_history: &[String]) -> Result<Option<PathBuf>> {
    // Prefill the last-used path; on a bad path, prefill what was just
    // typed so a typo can be fixed in place. Create the directory here so
    // an unusable path is caught immediately, not after every prompt.
    let mut initial = path_history.first().cloned().unwrap_or_else(|| ".".to_string());
    loop {
        let autocomplete = PathAutocomplete { history: path_history.to_vec() };
        let Some(path_input) = take_or_cancel(
            Text::new("导出到：")
                .with_initial_value(&initial)
                .with_autocomplete(autocomplete)
                .with_validator(required("路径不能为空"))
                .with_help_message("↑/↓：浏览历史 · Tab：填入 · 回车：确认 · Esc：取消")
                .prompt_skippable(),
            "导出路径",
        )? else {
            return Ok(None);
        };
        let trimmed = path_input.trim().to_string();
        let dir = PathBuf::from(expand_tilde(&trimmed));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => return Ok(Some(dir)),
            Err(e) => {
                eprintln!("无法使用 {}：{e}", dir.display());
                initial = trimmed; // re-prompt with the typed value
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(title: &str, date: Option<&str>) -> Episode {
        Episode {
            title: title.to_string(),
            torrent_url: "http://x/a.torrent".to_string(),
            size: Some(875_560_960),
            pub_date: date.map(str::to_string),
        }
    }

    #[test]
    fn fmt_size_mib_and_gib() {
        assert_eq!(fmt_size(875_560_960), "835.0 MiB");
        assert_eq!(fmt_size(2_147_483_648), "2.00 GiB");
    }

    #[test]
    fn row_shows_title_size_and_date() {
        let row = Row(ep("[组] 标题 [37]", Some("2026-06-28T23:10:00")));
        assert_eq!(row.to_string(), "[组] 标题 [37]  (835.0 MiB, 2026-06-28)");
    }

    #[test]
    fn row_handles_missing_size_and_date() {
        let mut episode = ep("t", None);
        episode.size = None;
        assert_eq!(Row(episode).to_string(), "t  (?, ?)");
    }

    #[test]
    fn row_survives_multibyte_pub_date() {
        // byte 10 falls inside a multibyte char — must not panic
        let row = Row(ep("t", Some("2026年06月28日")));
        assert_eq!(row.to_string(), "t  (835.0 MiB, ?)");
    }

    #[test]
    fn episode_summary_lists_titles_with_total_size() {
        // Two rows of 835.0 MiB each → 1.63 GiB total.
        let rows = [Row(ep("Anime - 01", None)), Row(ep("Anime - 02", None))];
        let opts: Vec<ListOption<&Row>> =
            rows.iter().enumerate().map(|(i, r)| ListOption::new(i, r)).collect();
        let summary = episode_selection_summary(&opts);
        assert!(summary.contains("2 集 · 1.63 GiB"), "was: {summary}");
        assert!(summary.contains("\n  • Anime - 01"), "was: {summary}");
        assert!(summary.contains("\n  • Anime - 02"), "was: {summary}");
    }

    #[test]
    fn episode_summary_has_no_plural_form_and_omits_unknown_size() {
        let mut episode = ep("Solo", None);
        episode.size = None;
        let row = Row(episode);
        let summary = episode_selection_summary(&[ListOption::new(0, &row)]);
        assert!(summary.contains("1 集"), "was: {summary}");
        assert!(!summary.contains('·'), "no size divider when unknown: {summary}");
    }

    #[test]
    fn autocomplete_filters_history_case_insensitively() {
        let mut ac = PathAutocomplete {
            history: vec!["/Users/alisa/Downloads".to_string(), "/tmp/other".to_string()],
        };
        assert_eq!(
            ac.get_suggestions("down").unwrap(),
            vec!["/Users/alisa/Downloads".to_string()]
        );
        assert_eq!(ac.get_suggestions("").unwrap().len(), 2);
    }

    #[test]
    fn autocomplete_completion_fills_highlighted_suggestion() {
        let mut ac = PathAutocomplete { history: vec![] };
        assert_eq!(
            ac.get_completion("x", Some("/a/b".to_string())).unwrap(),
            Some("/a/b".to_string())
        );
        assert_eq!(ac.get_completion("x", None).unwrap(), None);
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x"), format!("{home}/x"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs/x"), "/abs/x");
        assert_eq!(expand_tilde("rel/x"), "rel/x");
    }

    #[test]
    fn classify_pick_routes_esc_by_context() {
        // Esc (Ok(None)) goes back only when a previous step exists.
        assert!(matches!(classify_pick::<u8>(Ok(None), true), Ok(PickerAction::Back)));
        assert!(matches!(classify_pick::<u8>(Ok(None), false), Ok(PickerAction::Cancel)));
    }

    #[test]
    fn classify_pick_ctrl_c_always_cancels() {
        for can_go_back in [true, false] {
            assert!(matches!(
                classify_pick::<u8>(Err(InquireError::OperationInterrupted), can_go_back),
                Ok(PickerAction::Cancel)
            ));
        }
    }

    #[test]
    fn classify_pick_selection_and_empty() {
        assert!(matches!(
            classify_pick(Ok(Some(vec![1u8, 2])), true),
            Ok(PickerAction::Picked(v)) if v == vec![1, 2]
        ));
        assert!(matches!(
            classify_pick::<u8>(Ok(Some(vec![])), true),
            Ok(PickerAction::NothingTicked)
        ));
    }

    #[test]
    fn classify_pick_real_errors_propagate() {
        assert!(classify_pick::<u8>(Err(InquireError::NotTTY), false).is_err());
    }

    #[test]
    fn episode_picker_help_reflects_back_context() {
        assert!(episode_picker_help(true).ends_with("Esc：返回上一步"));
        assert!(episode_picker_help(false).ends_with("Esc：取消"));
    }
}
