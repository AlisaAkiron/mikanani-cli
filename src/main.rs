mod config;
mod download;
mod export;
mod feed;
mod mikan;
mod qbt;
mod sanitize;
mod select;

use anyhow::{Context, Result, bail};
use clap::Parser;
use inquire::autocompletion::Replacement;
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::{Autocomplete, CustomUserError, InquireError, MultiSelect, Password, PasswordDisplayMode, Text};
use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use download::Outcome;
use feed::Episode;

/// 一个用于下载 Mikan Project（蜜柑计划）番剧 RSS 订阅的交互式命令行工具。
#[derive(Parser)]
#[command(name = "mikan", version, about, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Mikan RSS 订阅链接，例如："https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
    url: Option<String>,

    /// 代理地址（例如 http://127.0.0.1:7890）。默认使用代理环境变量（所有平台）以及 macOS/Windows 系统代理。
    #[arg(long)]
    proxy: Option<String>,

    /// 非交互模式运行：不显示任何提示，需要同时指定一个选择参数和一个输出参数
    #[arg(short = 'y', long, help_heading = "非交互模式")]
    yes: bool,

    /// 选择订阅中的所有剧集
    #[arg(long, help_heading = "非交互模式")]
    all: bool,

    /// 只保留最新的 N 集
    #[arg(long, value_name = "N", help_heading = "非交互模式")]
    latest: Option<usize>,

    /// 只保留标题包含 TEXT 的剧集（不区分大小写）
    #[arg(long, value_name = "TEXT", help_heading = "非交互模式")]
    filter: Option<String>,

    /// 将 .torrent 文件下载到 DIR
    #[arg(long, value_name = "DIR", help_heading = "非交互模式")]
    out: Option<PathBuf>,

    /// 将种子 URL 列表写入 DIR
    #[arg(long = "url-list", value_name = "DIR", help_heading = "非交互模式")]
    url_list: Option<PathBuf>,

    /// 使用已保存的配置添加到 qBittorrent（不带值时使用「default」；--qbt=NAME 指定配置名）
    #[arg(
        long,
        value_name = "PROFILE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "default",
        help_heading = "非交互模式"
    )]
    qbt: Option<String>,

    /// qBittorrent 分类（默认为清洗后的订阅标题）
    #[arg(long, value_name = "NAME", help_heading = "非交互模式")]
    category: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// 管理 qBittorrent 连接配置
    Qbt {
        #[command(subcommand)]
        action: QbtAction,
    },
    /// 搜索蜜柑计划中的番剧并交互式选择剧集
    Search {
        /// 搜索关键词（留空则会提示输入）
        query: Option<String>,

        /// 代理地址（例如 http://127.0.0.1:7890）。默认使用代理环境变量以及 macOS/Windows 系统代理。
        #[arg(long)]
        proxy: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum QbtAction {
    /// 交互式创建或更新配置（默认名称为「default」）
    Set {
        /// 要创建或更新的配置（默认为「default」）
        name: Option<String>,
    },
    /// 列出已保存的配置
    List,
    /// 删除一个配置
    Remove {
        /// 要删除的配置
        name: String,
    },
    /// 使用配置连接并打印 qBittorrent 版本
    Test {
        /// 要测试的配置（默认为「default」）
        name: Option<String>,
    },
}

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

fn build_client(proxy: Option<&str>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("无效的 --proxy 地址")?);
    }
    builder.build().context("创建 HTTP 客户端")
}

fn add_proxy_hint(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}").to_lowercase();
    let looks_blocked = [
        "certificate", "tls", "ssl", "connect", "timed out", "dns",
        "bad gateway", "gateway timeout", "proxy",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    if looks_blocked {
        e.context(
            "无法访问订阅 —— mikanani.me 常被 DNS 污染；请检查代理\
             （--proxy http://host:port、HTTPS_PROXY 或系统代理）",
        )
    } else {
        e
    }
}

/// An animated one-line status printed to stderr until dropped, then the
/// line is cleared. Reassures the user during blocking network calls — the
/// feed fetch and qBittorrent connect can stall for seconds behind a proxy or
/// a wrong endpoint. A no-op when stderr isn't a terminal, so pipes and logs
/// stay clean.
struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn start(message: &str) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        if !std::io::stderr().is_terminal() {
            return Spinner { stop, handle: None };
        }
        let message = message.to_string();
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut err = std::io::stderr();
            let mut frame = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let _ = write!(err, "\r{} {message}", FRAMES[frame % FRAMES.len()]);
                let _ = err.flush();
                frame += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = write!(err, "\r\x1b[2K"); // clear the line on the way out
            let _ = err.flush();
        });
        Spinner { stop, handle: Some(handle) }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Progress lines share a fixed-width status column so disk and qBittorrent
/// outcomes stay aligned even when interleaved. `tag` is the `[i/n]` counter.
fn progress(tag: &str, status: &str, detail: impl fmt::Display) {
    println!("{tag} {status:<16} {detail}");
}

/// Like [`progress`] but to stderr, for failures.
fn progress_err(tag: &str, status: &str, detail: impl fmt::Display) {
    eprintln!("{tag} {status:<16} {detail}");
}

/// Connect to qBittorrent, showing a spinner during the blocking attempt.
fn connect_with_spinner(profile: &config::QbtProfile) -> Result<qbt::QbtClient> {
    let _spinner = Spinner::start("正在连接 qBittorrent…");
    qbt::QbtClient::connect(profile)
}

/// Prompt for a profile and connect, re-prompting with the last entry
/// prefilled on connection failure so a typo is trivial to fix. `initial`
/// prefills the first attempt (when updating an existing profile). Ok(None)
/// means the user cancelled.
fn prompt_and_connect(
    initial: Option<&config::QbtProfile>,
) -> Result<Option<(qbt::QbtClient, config::QbtProfile)>> {
    let mut prefill = initial.cloned();
    loop {
        let Some(profile) = prompt_profile(prefill.as_ref())? else {
            return Ok(None);
        };
        match connect_with_spinner(&profile) {
            Ok(client) => {
                println!("已连接 —— qBittorrent {}", client.version());
                return Ok(Some((client, profile)));
            }
            Err(e) => {
                eprintln!("错误：{e:#}");
                prefill = Some(profile);
            }
        }
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("错误：{e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let mut args = Args::parse();
    match args.command.take() {
        Some(Command::Qbt { action }) => return qbt_command(action),
        Some(Command::Search { query, proxy }) => return search_flow(query, proxy.as_deref()),
        None => {}
    }
    let Some(url) = args.url.clone() else {
        // Bare `mikan` launches search — unless headless flags/-y were given,
        // which still require an explicit URL (preserve the old usage errors).
        if args.yes {
            usage_error(
                "缺少必需的参数：\n  <URL>".to_string(),
            );
        }
        if let Some(flag) = headless_flag_present(&args) {
            usage_error(format!("{flag} 需要配合 --yes 使用（非交互模式）"));
        }
        return search_flow(None, args.proxy.as_deref());
    };
    if args.yes {
        if let Err(msg) = headless_precondition(&args) {
            usage_error(msg);
        }
        return headless(&url, &args);
    }
    // Headless-only flags without -y: reject rather than silently open the
    // wizard when the user clearly meant a non-interactive run.
    if let Some(flag) = headless_flag_present(&args) {
        usage_error(format!("{flag} 需要配合 --yes 使用（非交互模式）"));
    }
    wizard(&url, args.proxy.as_deref())
}

/// Emit `message` as a clap usage error (stderr + exit 2) and terminate.
fn usage_error(message: String) -> ! {
    use clap::CommandFactory;
    Args::command()
        .error(clap::error::ErrorKind::MissingRequiredArgument, message)
        .exit()
}

/// The first headless-only flag present, if any — used to reject headless
/// flags supplied without `-y`.
fn headless_flag_present(args: &Args) -> Option<&'static str> {
    if args.all {
        Some("--all")
    } else if args.latest.is_some() {
        Some("--latest")
    } else if args.filter.is_some() {
        Some("--filter")
    } else if args.out.is_some() {
        Some("--out")
    } else if args.url_list.is_some() {
        Some("--url-list")
    } else if args.qbt.is_some() {
        Some("--qbt")
    } else if args.category.is_some() {
        Some("--category")
    } else {
        None
    }
}

/// Check that a `-y` run names both what to select and where to send it. Err
/// carries the usage message to print (and exit 2 on).
fn headless_precondition(args: &Args) -> Result<(), String> {
    if !(args.all || args.latest.is_some() || args.filter.is_some()) {
        return Err("请指定 --all、--latest 或 --filter".to_string());
    }
    if args.out.is_none() && args.url_list.is_none() && args.qbt.is_none() {
        return Err("请至少指定 --out、--url-list、--qbt 之一".to_string());
    }
    Ok(())
}

/// Wizard step 4: resolve profile (inline creation when none saved),
/// connect, and ask the category. Ok(None) = user cancelled.
fn resolve_qbt(cfg: &config::Config, channel_title: &str) -> Result<Option<QbtSetup>> {
    let (client, save_as) = if cfg.qbt.is_empty() {
        println!("尚未配置 qBittorrent，现在来设置一个");
        let Some((client, profile)) = prompt_and_connect(None)? else {
            return Ok(None);
        };
        let Some(save) = take_or_cancel(
            inquire::Confirm::new("保存该配置以便下次使用？")
                .with_default(true)
                .prompt_skippable(),
            "是否保存配置",
        )? else {
            return Ok(None);
        };
        let save_as = if save {
            let Some(name) = take_or_cancel(
                Text::new("配置名称：")
                    .with_initial_value("default")
                    .with_validator(|input: &str| {
                        if input.trim().is_empty() {
                            Ok(Validation::Invalid("名称不能为空".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt_skippable(),
                "配置名称",
            )? else {
                return Ok(None);
            };
            Some((name.trim().to_string(), profile))
        } else {
            None
        };
        (client, save_as)
    } else {
        // One or more saved profiles: pick one (auto when there's only one),
        // connect, and on failure let the user retry or switch — never abort
        // the whole wizard and discard the episode/format/path choices.
        loop {
            let profile = if cfg.qbt.len() == 1 {
                let (name, profile) = cfg.qbt.iter().next().expect("len checked");
                println!("使用 qBittorrent 配置「{name}」");
                profile.clone()
            } else {
                let names: Vec<String> = cfg.qbt.keys().cloned().collect();
                let Some(name) = take_or_cancel(
                    inquire::Select::new("qBittorrent 配置：", names).prompt_skippable(),
                    "qBittorrent 配置",
                )? else {
                    return Ok(None);
                };
                cfg.qbt[&name].clone()
            };
            match connect_with_spinner(&profile) {
                Ok(client) => {
                    println!("已连接 —— qBittorrent {}", client.version());
                    break (client, None);
                }
                Err(e) => {
                    eprintln!("错误：{e:#}");
                    let Some(true) = take_or_cancel(
                        inquire::Confirm::new("连接失败 —— 重试？")
                            .with_default(true)
                            .prompt_skippable(),
                        "是否重试",
                    )? else {
                        return Ok(None);
                    };
                }
            }
        }
    };

    let Some(category) = take_or_cancel(
        Text::new("qBittorrent 分类（分组/文件夹）：")
            .with_initial_value(&default_category(channel_title))
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(Validation::Invalid("分类不能为空".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt_skippable(),
        "分类",
    )? else {
        return Ok(None);
    };

    Ok(Some(QbtSetup { client, category: category.trim().to_string(), save_as }))
}

/// Read a written/skipped .torrent back for upload; a read failure counts
/// as a qBt failure but never aborts the batch.
fn read_back_for_qbt(tag: &str, path: &std::path::Path, qbt_failed: &mut u32) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            *qbt_failed += 1;
            progress_err(tag, "失败", format!("回读 {}：{e}", path.display()));
            None
        }
    }
}

/// Running tally the per-episode batch produces.
#[derive(Default)]
struct Totals {
    downloaded: u32,
    skipped: u32,
    failed: u32,
    qbt_added: u32,
    qbt_failed: u32,
}

/// The per-episode batch. For each episode, optionally download the .torrent
/// to `torrents_dir` (when `Some`) and/or add it to qBittorrent (when
/// `qbt_setup` is `Some`), obtaining the bytes at most once and reusing them.
/// Prints one progress line per action; a per-episode failure is tallied and
/// never aborts the batch.
fn process_episodes(
    client: &reqwest::blocking::Client,
    episodes: &[Episode],
    torrents_dir: Option<&std::path::Path>,
    qbt_setup: Option<&QbtSetup>,
) -> Totals {
    let mut totals = Totals::default();
    let n = episodes.len();
    let width = n.to_string().len();
    for (i, ep) in episodes.iter().enumerate() {
        let tag = format!("[{:>width$}/{n}]", i + 1);
        let filename = sanitize::torrent_filename(&ep.title, &ep.torrent_url);
        // Obtain this episode's torrent bytes at most once.
        let mut bytes_for_qbt: Option<Vec<u8>> = None;
        match torrents_dir {
            Some(dir) => match download::download(client, ep, dir) {
                Outcome::Downloaded(path) => {
                    totals.downloaded += 1;
                    progress(&tag, "已下载", path.display());
                    if qbt_setup.is_some() {
                        bytes_for_qbt = read_back_for_qbt(&tag, &path, &mut totals.qbt_failed);
                    }
                }
                Outcome::Skipped(path) => {
                    totals.skipped += 1;
                    progress(&tag, "已跳过（已存在）", path.display());
                    if qbt_setup.is_some() {
                        bytes_for_qbt = read_back_for_qbt(&tag, &path, &mut totals.qbt_failed);
                    }
                }
                Outcome::Failed(message) => {
                    totals.failed += 1;
                    progress_err(&tag, "失败", message);
                }
            },
            None if qbt_setup.is_some() => match download::fetch_bytes(client, &ep.torrent_url) {
                Ok(bytes) => bytes_for_qbt = Some(bytes),
                Err(e) => {
                    totals.qbt_failed += 1;
                    progress_err(&tag, "失败", format!("{filename}: {e:#}"));
                }
            },
            None => {}
        }
        if let (Some(setup), Some(bytes)) = (qbt_setup, bytes_for_qbt) {
            match setup.client.add_torrent(&filename, bytes, &setup.category) {
                Ok(()) => {
                    totals.qbt_added += 1;
                    progress(&tag, "已添加到 qBittorrent", &filename);
                }
                Err(e) => {
                    totals.qbt_failed += 1;
                    progress_err(&tag, "失败", format!("{filename}: {e:#}"));
                }
            }
        }
    }
    totals
}

fn wizard(url: &str, proxy: Option<&str>) -> Result<i32> {
    let client = build_client(proxy)?;

    let feed = {
        let _spinner = Spinner::start("正在获取订阅…");
        feed::fetch_feed(&client, url).map_err(add_proxy_hint)?
    };
    if feed.episodes.is_empty() {
        bail!("订阅「{}」中没有剧集", feed.channel_title);
    }
    run_export_flow(&client, feed)
}

/// Interactive search entry point: resolve a query, pick a show, pick a
/// subtitle group, then hand the resulting feed to the shared export flow.
/// Prompt for a Mikan search term. `initial` pre-fills the input, so a
/// re-search can edit the previous term instead of retyping. Ok(None) means
/// the user cancelled; the returned string is trimmed and non-empty.
fn prompt_query(initial: Option<&str>) -> Result<Option<String>> {
    let mut input = Text::new("搜索蜜柑：")
        .with_validator(|s: &str| {
            if s.trim().is_empty() {
                Ok(Validation::Invalid("请输入搜索关键词".into()))
            } else {
                Ok(Validation::Valid)
            }
        })
        .with_help_message("回车：搜索 · Esc：取消");
    if let Some(init) = initial {
        input = input.with_initial_value(init);
    }
    Ok(take_or_cancel(input.prompt_skippable(), "搜索关键词")?.map(|q| q.trim().to_string()))
}

fn search_flow(query: Option<String>, proxy: Option<&str>) -> Result<i32> {
    let client = build_client(proxy)?;

    // Resolve the initial query (prompt when absent or blank).
    let mut query = match query {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => match prompt_query(None)? {
            Some(q) => q,
            None => {
                println!("已取消");
                return Ok(0);
            }
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
            match prompt_query(Some(&query))? {
                Some(q) => {
                    query = q;
                    continue 'search;
                }
                None => {
                    println!("已取消");
                    return Ok(0);
                }
            }
        }

        // The scraped shows, plus a trailing "search again" entry.
        let mut show_options: Vec<ShowChoice> =
            shows.iter().cloned().map(ShowChoice::Pick).collect();
        show_options.push(ShowChoice::SearchAgain);

        // Pick a show. "Back" from the subgroup step returns here without
        // re-searching.
        'show: loop {
            let Some(chosen) = take_or_cancel(
                inquire::Select::new("番剧：", show_options.clone())
                    .with_help_message("输入以筛选 · 回车：选择 · Esc：取消")
                    .prompt_skippable(),
                "番剧",
            )?
            else {
                println!("已取消");
                return Ok(0);
            };
            let show = match chosen {
                ShowChoice::SearchAgain => match prompt_query(Some(&query))? {
                    Some(q) => {
                        query = q;
                        continue 'search;
                    }
                    None => {
                        println!("已取消");
                        return Ok(0);
                    }
                },
                ShowChoice::Pick(show) => show,
            };

            // Fetch subtitle groups and decide the subgroup.
            let groups = {
                let _spinner = Spinner::start("正在加载字幕组…");
                mikan::subgroups(&client, show.id).map_err(add_proxy_hint)?
            };
            let subgroup_id = match subgroup_choice(groups) {
                SubgroupChoice::Auto(id) => id,
                SubgroupChoice::Menu(options) => {
                    let mut nav: Vec<GroupNav> = options.into_iter().map(GroupNav::Group).collect();
                    nav.push(GroupNav::Back);
                    let Some(chosen) = take_or_cancel(
                        inquire::Select::new("字幕组：", nav)
                            .with_help_message("输入以筛选 · 回车：选择 · Esc：取消")
                            .prompt_skippable(),
                        "字幕组",
                    )?
                    else {
                        println!("已取消");
                        return Ok(0);
                    };
                    match chosen {
                        GroupNav::Group(group) => group.id,
                        GroupNav::Back => continue 'show,
                    }
                }
            };

            // Build the RSS URL and reuse the shared export flow.
            let url = rss_url(show.id, subgroup_id);
            let feed = {
                let _spinner = Spinner::start("正在获取订阅…");
                feed::fetch_feed(&client, &url).map_err(add_proxy_hint)?
            };
            if feed.episodes.is_empty() {
                bail!("订阅「{}」中没有剧集", feed.channel_title);
            }
            return run_export_flow(&client, feed);
        }
    }
}

/// Runs the interactive picker → format → path → qBittorrent → execute steps
/// over an already-fetched feed. Shared by the URL wizard and the search flow.
fn run_export_flow(client: &reqwest::blocking::Client, feed: feed::Feed) -> Result<i32> {
    let feed::Feed { channel_title, episodes } = feed;

    // Step 1: pick episodes.
    let rows: Vec<Row> = select::sort_episodes(episodes).into_iter().map(Row).collect();
    let picked = match MultiSelect::new(&channel_title, rows)
        .with_page_size(15)
        .with_formatter(&|selected| episode_selection_summary(selected))
        .with_help_message("输入以筛选 · 空格：选中/取消 · →/←：全选/全不选 · 回车：下一步 · Esc：取消")
        .prompt_skippable()
    {
        Ok(Some(picked)) if !picked.is_empty() => picked,
        Ok(Some(_)) => {
            // Enter with nothing ticked — a deliberate "never mind".
            println!("未选择任何内容");
            return Ok(0);
        }
        Ok(None) | Err(InquireError::OperationInterrupted) => {
            // Esc or Ctrl-C — the same "cancelled" as every later step.
            println!("已取消");
            return Ok(0);
        }
        Err(e) => return Err(e).context("显示剧集选择器"),
    };
    let selected: Vec<Episode> = picked.into_iter().map(|row| row.0).collect();

    // Step 2: what to export.
    let options = vec![
        FormatOption {
            format: ExportFormat::TorrentFiles,
            label: "下载 .torrent 文件".to_string(),
        },
        FormatOption {
            format: ExportFormat::UrlList,
            label: format!("将种子 URL 列表写入「{}」", export::url_list_filename(&channel_title)),
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
        println!("已取消");
        return Ok(0);
    };
    let want_torrents = formats.iter().any(|o| o.format == ExportFormat::TorrentFiles);
    let want_urls = formats.iter().any(|o| o.format == ExportFormat::UrlList);
    let want_qbt = formats.iter().any(|o| o.format == ExportFormat::Qbt);

    // Step 3: export path — only when something is written to disk.
    let config_dir = config::config_dir();
    let mut cfg = config::Config::load(&config_dir)?;
    let export_dir: Option<PathBuf> = if want_torrents || want_urls {
        // Prefill the last-used path; on a bad path, prefill what was just
        // typed so a typo can be fixed in place. Create the directory here so
        // an unusable path is caught immediately, not after every prompt.
        let mut initial = cfg
            .path_history
            .first()
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        loop {
            let autocomplete = PathAutocomplete { history: cfg.path_history.clone() };
            let Some(path_input) = take_or_cancel(
                Text::new("导出到：")
                    .with_initial_value(&initial)
                    .with_autocomplete(autocomplete)
                    .with_validator(|input: &str| {
                        if input.trim().is_empty() {
                            Ok(Validation::Invalid("路径不能为空".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .with_help_message("↑/↓：浏览历史 · Tab：填入 · 回车：确认 · Esc：取消")
                    .prompt_skippable(),
                "导出路径",
            )? else {
                println!("已取消");
                return Ok(0);
            };
            let trimmed = path_input.trim().to_string();
            let dir = PathBuf::from(expand_tilde(&trimmed));
            match std::fs::create_dir_all(&dir) {
                Ok(()) => break Some(dir),
                Err(e) => {
                    eprintln!("无法使用 {}：{e}", dir.display());
                    initial = trimmed; // re-prompt with the typed value
                }
            }
        }
    } else {
        None
    };

    // Step 4: qBt profile + category.
    let qbt_setup = if want_qbt {
        match resolve_qbt(&cfg, &channel_title)? {
            Some(setup) => Some(setup),
            None => {
                println!("已取消");
                return Ok(0);
            }
        }
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

    // Downloading .torrent files to disk is gated on `want_torrents`; adding
    // to qBittorrent is gated on `qbt_setup`. Hand the loop those two facts.
    let torrents_dir = if want_torrents { export_dir.as_deref() } else { None };
    let totals = process_episodes(client, &selected, torrents_dir, qbt_setup.as_ref());
    if want_torrents {
        println!(
            "已下载 {}，已跳过 {}，失败 {}",
            totals.downloaded, totals.skipped, totals.failed
        );
    }
    if want_qbt {
        println!("已添加到 qBittorrent {}，失败 {}", totals.qbt_added, totals.qbt_failed);
    }

    let mut url_list = None;
    let mut url_failed = false;
    if want_urls {
        let dir = export_dir.as_ref().expect("path was asked when urls are exported");
        match export::write_url_list(&selected, dir, &channel_title) {
            Ok(path) => {
                println!("URL 列表已写入 {}", path.display());
                url_list = Some(path);
            }
            Err(e) => {
                url_failed = true;
                eprintln!("失败           URL 列表：{e:#}");
            }
        }
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

    let any_failure = totals.failed > 0 || totals.qbt_failed > 0 || url_failed;
    Ok(if any_failure { 1 } else { 0 })
}

/// Non-interactive counterpart to `wizard`: no prompts, driven entirely by
/// flags. Preconditions (a selection flag and an output flag) are checked in
/// `run` before this is called. Reads qBittorrent profiles from config but
/// never writes config.
fn headless(url: &str, args: &Args) -> Result<i32> {
    let client = build_client(args.proxy.as_deref())?;
    let feed::Feed { channel_title, episodes } = {
        let _spinner = Spinner::start("正在获取订阅…");
        feed::fetch_feed(&client, url).map_err(add_proxy_hint)?
    };
    if episodes.is_empty() {
        bail!("订阅「{channel_title}」中没有剧集");
    }

    let selected = select::select(episodes, args.latest, args.filter.as_deref());
    if selected.is_empty() {
        println!("没有匹配的剧集");
        return Ok(0);
    }
    println!("已选择 {} 集", selected.len());

    // Resolve the qBittorrent profile read-only — no inline creation, no
    // prompts (this may run headless in cron).
    let qbt_setup = match &args.qbt {
        Some(profile_name) => {
            let cfg = config::Config::load(&config::config_dir())?;
            let profile = match cfg.qbt.get(profile_name) {
                Some(profile) => profile.clone(),
                None if cfg.qbt.is_empty() => {
                    bail!("没有名为「{profile_name}」的配置 —— 使用 mikan qbt set 添加")
                }
                None => {
                    let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                    bail!("没有名为「{profile_name}」的配置 —— 已保存的配置：{}", names.join(", "));
                }
            };
            let client = connect_with_spinner(&profile)?;
            println!("已连接 —— qBittorrent {}", client.version());
            let category = args
                .category
                .clone()
                .unwrap_or_else(|| default_category(&channel_title));
            client.ensure_category(&category)?;
            Some(QbtSetup { client, category, save_as: None })
        }
        None => None,
    };

    for dir in [args.out.as_ref(), args.url_list.as_ref()].into_iter().flatten() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建 {}", dir.display()))?;
    }

    let totals = process_episodes(&client, &selected, args.out.as_deref(), qbt_setup.as_ref());
    if args.out.is_some() {
        println!(
            "已下载 {}，已跳过 {}，失败 {}",
            totals.downloaded, totals.skipped, totals.failed
        );
    }
    if args.qbt.is_some() {
        println!("已添加到 qBittorrent {}，失败 {}", totals.qbt_added, totals.qbt_failed);
    }

    let mut url_failed = false;
    if let Some(dir) = &args.url_list {
        match export::write_url_list(&selected, dir, &channel_title) {
            Ok(path) => println!("URL 列表已写入 {}", path.display()),
            Err(e) => {
                url_failed = true;
                eprintln!("失败           URL 列表：{e:#}");
            }
        }
    }

    let any_failure = totals.failed > 0 || totals.qbt_failed > 0 || url_failed;
    Ok(if any_failure { 1 } else { 0 })
}

/// Collapses inquire's two cancellation signals (Esc ⇒ Ok(None), Ctrl-C ⇒
/// OperationInterrupted) into Ok(None); real errors keep their context.
fn take_or_cancel<T>(
    result: Result<Option<T>, InquireError>,
    what: &str,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(value),
        Err(InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("获取{what}")),
    }
}

/// Prompt endpoint/username/password for a qBt profile. `existing`
/// prefills the prompts when updating. Returns Ok(None) on cancel.
fn prompt_profile(existing: Option<&config::QbtProfile>) -> Result<Option<config::QbtProfile>> {
    let default_endpoint = existing
        .map(|p| p.endpoint.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let Some(endpoint) = take_or_cancel(
        Text::new("qBittorrent WebUI 地址：")
            .with_initial_value(&default_endpoint)
            .with_validator(|input: &str| {
                let input = input.trim();
                if input.starts_with("http://") || input.starts_with("https://") {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid("必须以 http:// 或 https:// 开头".into()))
                }
            })
            .prompt_skippable(),
        "qBittorrent 地址",
    )? else {
        return Ok(None);
    };
    let Some(username) = take_or_cancel(
        Text::new("用户名（留空则使用 qBittorrent 的本机免认证）：")
            .with_initial_value(existing.map(|p| p.username.as_str()).unwrap_or(""))
            .prompt_skippable(),
        "用户名",
    )? else {
        return Ok(None);
    };
    let password = if username.trim().is_empty() {
        String::new()
    } else {
        let Some(password) = take_or_cancel(
            Password::new("密码：")
                .without_confirmation()
                .with_display_mode(PasswordDisplayMode::Masked)
                .prompt_skippable(),
            "密码",
        )? else {
            return Ok(None);
        };
        password
    };
    Ok(Some(config::QbtProfile {
        endpoint: endpoint.trim().trim_end_matches('/').to_string(),
        username: username.trim().to_string(),
        password,
    }))
}

/// Default qBt category for an import: the sanitized feed title.
fn default_category(channel_title: &str) -> String {
    let stem = sanitize::sanitize_stem(channel_title);
    if stem.is_empty() { "mikan".to_string() } else { stem }
}

/// Optional CLI profile name: default when absent, trimmed and non-empty
/// when given.
fn profile_name_or_default(name: Option<String>) -> Result<String> {
    match name {
        None => Ok("default".to_string()),
        Some(n) => {
            let n = n.trim().to_string();
            if n.is_empty() {
                bail!("配置名称不能为空");
            }
            Ok(n)
        }
    }
}

/// Everything step 4 resolves: a connected client, the target category,
/// and — for an inline-created profile the user chose to save — its name.
struct QbtSetup {
    client: qbt::QbtClient,
    category: String,
    save_as: Option<(String, config::QbtProfile)>,
}

fn qbt_command(action: QbtAction) -> Result<i32> {
    let dir = config::config_dir();
    let mut cfg = config::Config::load(&dir)?;
    match action {
        QbtAction::Set { name } => {
            let name = profile_name_or_default(name)?;
            let Some((_client, profile)) = prompt_and_connect(cfg.qbt.get(&name))? else {
                println!("已取消");
                return Ok(0);
            };
            cfg.qbt.insert(name.clone(), profile);
            cfg.save(&dir).context("保存配置")?;
            println!("配置「{name}」已保存");
            Ok(0)
        }
        QbtAction::List => {
            if cfg.qbt.is_empty() {
                println!("暂无配置 —— 使用 mikan qbt set 添加");
            }
            for (name, profile) in &cfg.qbt {
                let user = if profile.username.is_empty() { "（免认证）" } else { profile.username.as_str() };
                println!("{name}: {} {user}", profile.endpoint);
            }
            Ok(0)
        }
        QbtAction::Remove { name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                bail!("配置名称不能为空");
            }
            if cfg.qbt.remove(&name).is_none() {
                if cfg.qbt.is_empty() {
                    bail!("没有名为「{name}」的配置 —— 暂无已保存的配置");
                }
                let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                bail!("没有名为「{name}」的配置 —— 已保存的配置：{}", names.join(", "));
            }
            cfg.save(&dir).context("保存配置")?;
            println!("配置「{name}」已删除");
            Ok(0)
        }
        QbtAction::Test { name } => {
            let name = profile_name_or_default(name)?;
            let profile = match cfg.qbt.get(&name) {
                Some(profile) => profile,
                None if cfg.qbt.is_empty() => {
                    bail!("没有名为「{name}」的配置 —— 使用 mikan qbt set 添加")
                }
                None => {
                    let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                    bail!("没有名为「{name}」的配置 —— 已保存的配置：{}", names.join(", "))
                }
            };
            let client = connect_with_spinner(profile)?;
            println!("已连接 —— qBittorrent {} @ {}", client.version(), profile.endpoint);
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::Episode;

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
    fn build_client_rejects_bad_proxy() {
        assert!(build_client(Some("not a url")).is_err());
    }

    #[test]
    fn build_client_accepts_http_proxy() {
        assert!(build_client(Some("http://127.0.0.1:7890")).is_ok());
    }

    #[test]
    fn proxy_hint_added_for_gateway_errors() {
        let err = anyhow::anyhow!("HTTP status server error (502 Bad Gateway) for url (http://x/)");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(hinted.contains("检查代理"), "was: {hinted}");
    }

    #[test]
    fn proxy_hint_added_for_connect_errors() {
        let err = anyhow::anyhow!("error sending request: connection refused");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(hinted.contains("检查代理"), "was: {hinted}");
    }

    #[test]
    fn no_proxy_hint_for_plain_http_404() {
        let err = anyhow::anyhow!("HTTP status client error (404 Not Found) for url (https://mikanani.me/RSS/x)");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(!hinted.contains("检查代理"), "was: {hinted}");
    }

    #[test]
    fn row_survives_multibyte_pub_date() {
        // byte 10 falls inside a multibyte char — must not panic
        let row = Row(ep("t", Some("2026年06月28日")));
        assert_eq!(row.to_string(), "t  (835.0 MiB, ?)");
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
    fn cli_parses_wizard_and_qbt_subcommands() {
        let args = Args::try_parse_from(["mikan", "https://example.com/rss"]).unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.url.as_deref(), Some("https://example.com/rss"));

        let args = Args::try_parse_from(["mikan", "qbt", "set", "seedbox"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Qbt { action: QbtAction::Set { name: Some(ref n) } }) if n == "seedbox"
        ));
        assert!(Args::try_parse_from(["mikan", "qbt", "set"]).is_ok());
        assert!(Args::try_parse_from(["mikan", "qbt", "list"]).is_ok());
        assert!(Args::try_parse_from(["mikan", "qbt", "remove", "x"]).is_ok());
        assert!(Args::try_parse_from(["mikan", "qbt", "test"]).is_ok());
        assert!(Args::try_parse_from(["mikan", "qbt", "remove"]).is_err());

        // Bare `mikan` parses (both None); run() now routes this into the
        // interactive search flow rather than a usage error. `-y`/a headless
        // flag without a URL still produces the clap usage error.
        let bare = Args::try_parse_from(["mikan"]).unwrap();
        assert!(bare.command.is_none() && bare.url.is_none());
    }

    #[test]
    fn default_category_sanitizes_and_falls_back() {
        assert_eq!(default_category("Mikan Project - 石纪元"), "Mikan Project - 石纪元");
        assert_eq!(default_category("A / B"), "A ⁄ B");
        assert_eq!(default_category("..."), "mikan");
    }

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
    fn cli_parses_headless_flags() {
        let a = Args::try_parse_from(["mikan", "http://x/rss", "-y", "--all", "--qbt"]).unwrap();
        assert!(a.yes && a.all);
        assert_eq!(a.qbt.as_deref(), Some("default")); // bare --qbt → default profile

        let a =
            Args::try_parse_from(["mikan", "http://x/rss", "-y", "--latest", "3", "--out", "/tmp/x"])
                .unwrap();
        assert_eq!(a.latest, Some(3));
        assert_eq!(a.out.as_deref(), Some(std::path::Path::new("/tmp/x")));

        let a = Args::try_parse_from([
            "mikan", "http://x/rss", "-y", "--filter", "1080p", "--qbt=seedbox",
        ])
        .unwrap();
        assert_eq!(a.filter.as_deref(), Some("1080p"));
        assert_eq!(a.qbt.as_deref(), Some("seedbox")); // --qbt=NAME names the profile
    }

    #[test]
    fn headless_precondition_requires_selection_and_output() {
        let a = Args::try_parse_from(["mikan", "http://x/rss", "-y", "--all"]).unwrap();
        assert_eq!(
            headless_precondition(&a).unwrap_err(),
            "请至少指定 --out、--url-list、--qbt 之一"
        );

        let a = Args::try_parse_from(["mikan", "http://x/rss", "-y", "--qbt"]).unwrap();
        assert_eq!(headless_precondition(&a).unwrap_err(), "请指定 --all、--latest 或 --filter");

        let a = Args::try_parse_from(["mikan", "http://x/rss", "-y", "--all", "--qbt"]).unwrap();
        assert!(headless_precondition(&a).is_ok());
    }

    #[test]
    fn headless_flag_without_yes_is_detected() {
        let a = Args::try_parse_from(["mikan", "http://x/rss", "--all"]).unwrap();
        assert_eq!(headless_flag_present(&a), Some("--all"));

        let a = Args::try_parse_from(["mikan", "http://x/rss"]).unwrap();
        assert_eq!(headless_flag_present(&a), None);
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

    #[test]
    fn cli_parses_search_subcommand() {
        let a = Args::try_parse_from(["mikan", "search", "dr.stone"]).unwrap();
        assert!(matches!(
            a.command,
            Some(Command::Search { query: Some(ref q), proxy: None }) if q == "dr.stone"
        ));

        let a = Args::try_parse_from(["mikan", "search"]).unwrap();
        assert!(matches!(a.command, Some(Command::Search { query: None, proxy: None })));

        let a = Args::try_parse_from(["mikan", "search", "x", "--proxy", "http://127.0.0.1:7890"])
            .unwrap();
        assert!(matches!(
            a.command,
            Some(Command::Search { proxy: Some(ref p), .. }) if p == "http://127.0.0.1:7890"
        ));
    }
}
