mod config;
mod download;
mod export;
mod feed;
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

/// Interactive downloader for Mikan Project RSS feeds.
#[derive(Parser)]
#[command(name = "mikan", version, about, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Mikan RSS feed URL, e.g. "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
    url: Option<String>,

    /// Proxy URL (e.g. http://127.0.0.1:7890). Defaults to proxy env vars
    /// (all platforms) and the macOS/Windows system proxy.
    #[arg(long)]
    proxy: Option<String>,

    /// Run non-interactively: no prompts. Needs a selection and an output flag
    #[arg(short = 'y', long, help_heading = "Non-interactive mode")]
    yes: bool,

    /// Select every episode in the feed
    #[arg(long, help_heading = "Non-interactive mode")]
    all: bool,

    /// Keep only the newest N episodes
    #[arg(long, value_name = "N", help_heading = "Non-interactive mode")]
    latest: Option<usize>,

    /// Keep episodes whose title contains TEXT (case-insensitive)
    #[arg(long, value_name = "TEXT", help_heading = "Non-interactive mode")]
    filter: Option<String>,

    /// Download .torrent files into DIR
    #[arg(long, value_name = "DIR", help_heading = "Non-interactive mode")]
    out: Option<PathBuf>,

    /// Write the torrent-URL list into DIR
    #[arg(long = "url-list", value_name = "DIR", help_heading = "Non-interactive mode")]
    url_list: Option<PathBuf>,

    /// Add to qBittorrent using a saved profile (bare = "default"; --qbt=NAME)
    #[arg(
        long,
        value_name = "PROFILE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "default",
        help_heading = "Non-interactive mode"
    )]
    qbt: Option<String>,

    /// qBittorrent category (default: sanitized feed title)
    #[arg(long, value_name = "NAME", help_heading = "Non-interactive mode")]
    category: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Manage qBittorrent connection profiles
    Qbt {
        #[command(subcommand)]
        action: QbtAction,
    },
}

#[derive(clap::Subcommand)]
enum QbtAction {
    /// Create or update a profile interactively (default name: "default")
    Set {
        /// Profile to create or update (default: "default")
        name: Option<String>,
    },
    /// List saved profiles
    List,
    /// Remove a profile
    Remove {
        /// Profile to remove
        name: String,
    },
    /// Connect with a profile and print the qBittorrent version
    Test {
        /// Profile to test (default: "default")
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
    let mut header = format!("\n{count} episode{}", if count == 1 { "" } else { "s" });
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

fn build_client(proxy: Option<&str>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid --proxy URL")?);
    }
    builder.build().context("building HTTP client")
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
            "could not reach the feed — mikanani.me is often DNS-poisoned; \
             check your proxy (--proxy http://host:port, HTTPS_PROXY, or the system proxy)",
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
    let _spinner = Spinner::start("connecting to qBittorrent…");
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
                println!("connected — qBittorrent {}", client.version());
                return Ok(Some((client, profile)));
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                prefill = Some(profile);
            }
        }
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let mut args = Args::parse();
    if let Some(Command::Qbt { action }) = args.command.take() {
        return qbt_command(action);
    }
    let Some(url) = args.url.clone() else {
        // A subcommand can't participate in required_unless_present
        // (clap 4.6 debug-asserts on it), so bare `mikan` is turned
        // into a proper clap usage error here: stderr + exit 2.
        usage_error("the following required arguments were not provided:\n  <URL>".to_string());
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
        usage_error(format!("{flag} requires --yes for non-interactive mode"));
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
        return Err("specify --all, --latest, or --filter".to_string());
    }
    if args.out.is_none() && args.url_list.is_none() && args.qbt.is_none() {
        return Err("specify at least one of --out, --url-list, --qbt".to_string());
    }
    Ok(())
}

/// Wizard step 4: resolve profile (inline creation when none saved),
/// connect, and ask the category. Ok(None) = user cancelled.
fn resolve_qbt(cfg: &config::Config, channel_title: &str) -> Result<Option<QbtSetup>> {
    let (client, save_as) = if cfg.qbt.is_empty() {
        println!("no qBittorrent profile configured — set one up now");
        let Some((client, profile)) = prompt_and_connect(None)? else {
            return Ok(None);
        };
        let Some(save) = take_or_cancel(
            inquire::Confirm::new("Save this profile for next time?")
                .with_default(true)
                .prompt_skippable(),
            "whether to save the profile",
        )? else {
            return Ok(None);
        };
        let save_as = if save {
            let Some(name) = take_or_cancel(
                Text::new("Profile name:")
                    .with_initial_value("default")
                    .with_validator(|input: &str| {
                        if input.trim().is_empty() {
                            Ok(Validation::Invalid("name must not be empty".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .prompt_skippable(),
                "the profile name",
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
                println!("using qBittorrent profile \"{name}\"");
                profile.clone()
            } else {
                let names: Vec<String> = cfg.qbt.keys().cloned().collect();
                let Some(name) = take_or_cancel(
                    inquire::Select::new("qBittorrent profile:", names).prompt_skippable(),
                    "the qBittorrent profile",
                )? else {
                    return Ok(None);
                };
                cfg.qbt[&name].clone()
            };
            match connect_with_spinner(&profile) {
                Ok(client) => {
                    println!("connected — qBittorrent {}", client.version());
                    break (client, None);
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    let Some(true) = take_or_cancel(
                        inquire::Confirm::new("couldn't connect — try again?")
                            .with_default(true)
                            .prompt_skippable(),
                        "whether to retry",
                    )? else {
                        return Ok(None);
                    };
                }
            }
        }
    };

    let Some(category) = take_or_cancel(
        Text::new("qBittorrent category (group/folder):")
            .with_initial_value(&default_category(channel_title))
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(Validation::Invalid("category must not be empty".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt_skippable(),
        "the category",
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
            progress_err(tag, "failed", format!("reading back {}: {e}", path.display()));
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
                    progress(&tag, "downloaded", path.display());
                    if qbt_setup.is_some() {
                        bytes_for_qbt = read_back_for_qbt(&tag, &path, &mut totals.qbt_failed);
                    }
                }
                Outcome::Skipped(path) => {
                    totals.skipped += 1;
                    progress(&tag, "skipped (exists)", path.display());
                    if qbt_setup.is_some() {
                        bytes_for_qbt = read_back_for_qbt(&tag, &path, &mut totals.qbt_failed);
                    }
                }
                Outcome::Failed(message) => {
                    totals.failed += 1;
                    progress_err(&tag, "failed", message);
                }
            },
            None if qbt_setup.is_some() => match download::fetch_bytes(client, &ep.torrent_url) {
                Ok(bytes) => bytes_for_qbt = Some(bytes),
                Err(e) => {
                    totals.qbt_failed += 1;
                    progress_err(&tag, "failed", format!("{filename}: {e:#}"));
                }
            },
            None => {}
        }
        if let (Some(setup), Some(bytes)) = (qbt_setup, bytes_for_qbt) {
            match setup.client.add_torrent(&filename, bytes, &setup.category) {
                Ok(()) => {
                    totals.qbt_added += 1;
                    progress(&tag, "added to qbt", &filename);
                }
                Err(e) => {
                    totals.qbt_failed += 1;
                    progress_err(&tag, "failed", format!("{filename}: {e:#}"));
                }
            }
        }
    }
    totals
}

fn wizard(url: &str, proxy: Option<&str>) -> Result<i32> {
    let client = build_client(proxy)?;

    let feed::Feed { channel_title, episodes } = {
        let _spinner = Spinner::start("fetching feed…");
        feed::fetch_feed(&client, url).map_err(add_proxy_hint)?
    };
    if episodes.is_empty() {
        bail!("feed \"{channel_title}\" contains no episodes");
    }

    // Step 1: pick episodes.
    let rows: Vec<Row> = select::sort_episodes(episodes).into_iter().map(Row).collect();
    let picked = match MultiSelect::new(&channel_title, rows)
        .with_page_size(15)
        .with_formatter(&|selected| episode_selection_summary(selected))
        .with_help_message("type to filter · space: toggle · →/←: all/none · enter: next · esc: cancel")
        .prompt_skippable()
    {
        Ok(Some(picked)) if !picked.is_empty() => picked,
        Ok(Some(_)) => {
            // Enter with nothing ticked — a deliberate "never mind".
            println!("nothing selected");
            return Ok(0);
        }
        Ok(None) | Err(InquireError::OperationInterrupted) => {
            // Esc or Ctrl-C — the same "cancelled" as every later step.
            println!("cancelled");
            return Ok(0);
        }
        Err(e) => return Err(e).context("showing the episode picker"),
    };
    let selected: Vec<Episode> = picked.into_iter().map(|row| row.0).collect();

    // Step 2: what to export.
    let options = vec![
        FormatOption {
            format: ExportFormat::TorrentFiles,
            label: "Download .torrent files".to_string(),
        },
        FormatOption {
            format: ExportFormat::UrlList,
            label: format!("Write torrent URLs to \"{}\"", export::url_list_filename(&channel_title)),
        },
        FormatOption {
            format: ExportFormat::Qbt,
            label: "Add to qBittorrent".to_string(),
        },
    ];
    let Some(formats) = take_or_cancel(
        MultiSelect::new("Export as:", options)
            .with_default(&[0])
            .with_validator(|opts: &[ListOption<&FormatOption>]| {
                if opts.is_empty() {
                    Ok(Validation::Invalid("select at least one format".into()))
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
            .with_help_message("space: toggle · enter: next · esc: cancel")
            .prompt_skippable(),
        "export formats",
    )? else {
        println!("cancelled");
        return Ok(0);
    };
    let want_torrents = formats.iter().any(|o| o.format == ExportFormat::TorrentFiles);
    let want_urls = formats.iter().any(|o| o.format == ExportFormat::UrlList);
    let want_qbt = formats.iter().any(|o| o.format == ExportFormat::Qbt);

    // Step 3: export path — only when something is written to disk.
    let config_dir = config::config_dir();
    let mut cfg = config::Config::load(&config_dir);
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
                Text::new("Export to:")
                    .with_initial_value(&initial)
                    .with_autocomplete(autocomplete)
                    .with_validator(|input: &str| {
                        if input.trim().is_empty() {
                            Ok(Validation::Invalid("path must not be empty".into()))
                        } else {
                            Ok(Validation::Valid)
                        }
                    })
                    .with_help_message("↑/↓: browse history · tab: fill · enter: confirm · esc: cancel")
                    .prompt_skippable(),
                "the export path",
            )? else {
                println!("cancelled");
                return Ok(0);
            };
            let trimmed = path_input.trim().to_string();
            let dir = PathBuf::from(expand_tilde(&trimmed));
            match std::fs::create_dir_all(&dir) {
                Ok(()) => break Some(dir),
                Err(e) => {
                    eprintln!("cannot use {}: {e}", dir.display());
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
                println!("cancelled");
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
                eprintln!("warning: could not save the qBittorrent profile: {e:#}");
            } else {
                println!("profile \"{name}\" saved");
            }
        }
        setup.client.ensure_category(&setup.category)?;
    }

    // Downloading .torrent files to disk is gated on `want_torrents`; adding
    // to qBittorrent is gated on `qbt_setup`. Hand the loop those two facts.
    let torrents_dir = if want_torrents { export_dir.as_deref() } else { None };
    let totals = process_episodes(&client, &selected, torrents_dir, qbt_setup.as_ref());
    if want_torrents {
        println!(
            "{} downloaded, {} skipped, {} failed",
            totals.downloaded, totals.skipped, totals.failed
        );
    }
    if want_qbt {
        println!("{} added to qBittorrent, {} failed", totals.qbt_added, totals.qbt_failed);
    }

    let mut url_list = None;
    let mut url_failed = false;
    if want_urls {
        let dir = export_dir.as_ref().expect("path was asked when urls are exported");
        match export::write_url_list(&selected, dir, &channel_title) {
            Ok(path) => {
                println!("urls written to {}", path.display());
                url_list = Some(path);
            }
            Err(e) => {
                url_failed = true;
                eprintln!("failed           url list: {e:#}");
            }
        }
    }

    // Path history: only when the path step actually ran and produced.
    if let Some(dir) = &export_dir {
        let any_disk_success = totals.downloaded > 0 || totals.skipped > 0 || url_list.is_some();
        if any_disk_success {
            cfg.record_path(&dir.to_string_lossy());
            if let Err(e) = cfg.save(&config_dir) {
                eprintln!("warning: could not save path history: {e:#}");
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
        let _spinner = Spinner::start("fetching feed…");
        feed::fetch_feed(&client, url).map_err(add_proxy_hint)?
    };
    if episodes.is_empty() {
        bail!("feed \"{channel_title}\" contains no episodes");
    }

    let selected = select::select(episodes, args.latest, args.filter.as_deref());
    if selected.is_empty() {
        println!("no episodes matched");
        return Ok(0);
    }
    println!(
        "selected {} episode{}",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );

    // Resolve the qBittorrent profile read-only — no inline creation, no
    // prompts (this may run headless in cron).
    let qbt_setup = match &args.qbt {
        Some(profile_name) => {
            let cfg = config::Config::load(&config::config_dir());
            let profile = match cfg.qbt.get(profile_name) {
                Some(profile) => profile.clone(),
                None if cfg.qbt.is_empty() => {
                    bail!("no such profile: {profile_name} — add one with: mikan qbt set")
                }
                None => {
                    let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                    bail!("no such profile: {profile_name} — saved profiles: {}", names.join(", "));
                }
            };
            let client = connect_with_spinner(&profile)?;
            println!("connected — qBittorrent {}", client.version());
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
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let totals = process_episodes(&client, &selected, args.out.as_deref(), qbt_setup.as_ref());
    if args.out.is_some() {
        println!(
            "{} downloaded, {} skipped, {} failed",
            totals.downloaded, totals.skipped, totals.failed
        );
    }
    if args.qbt.is_some() {
        println!("{} added to qBittorrent, {} failed", totals.qbt_added, totals.qbt_failed);
    }

    let mut url_failed = false;
    if let Some(dir) = &args.url_list {
        match export::write_url_list(&selected, dir, &channel_title) {
            Ok(path) => println!("urls written to {}", path.display()),
            Err(e) => {
                url_failed = true;
                eprintln!("failed           url list: {e:#}");
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
        Err(e) => Err(e).with_context(|| format!("asking for {what}")),
    }
}

/// Prompt endpoint/username/password for a qBt profile. `existing`
/// prefills the prompts when updating. Returns Ok(None) on cancel.
fn prompt_profile(existing: Option<&config::QbtProfile>) -> Result<Option<config::QbtProfile>> {
    let default_endpoint = existing
        .map(|p| p.endpoint.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let Some(endpoint) = take_or_cancel(
        Text::new("qBittorrent WebUI endpoint:")
            .with_initial_value(&default_endpoint)
            .with_validator(|input: &str| {
                let input = input.trim();
                if input.starts_with("http://") || input.starts_with("https://") {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid("must start with http:// or https://".into()))
                }
            })
            .prompt_skippable(),
        "the qBittorrent endpoint",
    )? else {
        return Ok(None);
    };
    let Some(username) = take_or_cancel(
        Text::new("Username (leave empty for qBt's localhost auth bypass):")
            .with_initial_value(existing.map(|p| p.username.as_str()).unwrap_or(""))
            .prompt_skippable(),
        "the username",
    )? else {
        return Ok(None);
    };
    let password = if username.trim().is_empty() {
        String::new()
    } else {
        let Some(password) = take_or_cancel(
            Password::new("Password:")
                .without_confirmation()
                .with_display_mode(PasswordDisplayMode::Masked)
                .prompt_skippable(),
            "the password",
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
                bail!("profile name must not be empty");
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
    let mut cfg = config::Config::load(&dir);
    match action {
        QbtAction::Set { name } => {
            let name = profile_name_or_default(name)?;
            let Some((_client, profile)) = prompt_and_connect(cfg.qbt.get(&name))? else {
                println!("cancelled");
                return Ok(0);
            };
            cfg.qbt.insert(name.clone(), profile);
            cfg.save(&dir).context("saving config")?;
            println!("profile \"{name}\" saved");
            Ok(0)
        }
        QbtAction::List => {
            if cfg.qbt.is_empty() {
                println!("no profiles — add one with: mikan qbt set");
            }
            for (name, profile) in &cfg.qbt {
                let user = if profile.username.is_empty() { "(no auth)" } else { profile.username.as_str() };
                println!("{name}: {} {user}", profile.endpoint);
            }
            Ok(0)
        }
        QbtAction::Remove { name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                bail!("profile name must not be empty");
            }
            if cfg.qbt.remove(&name).is_none() {
                if cfg.qbt.is_empty() {
                    bail!("no such profile: {name} — there are no saved profiles");
                }
                let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                bail!("no such profile: {name} — saved profiles: {}", names.join(", "));
            }
            cfg.save(&dir).context("saving config")?;
            println!("profile \"{name}\" removed");
            Ok(0)
        }
        QbtAction::Test { name } => {
            let name = profile_name_or_default(name)?;
            let profile = match cfg.qbt.get(&name) {
                Some(profile) => profile,
                None if cfg.qbt.is_empty() => {
                    bail!("no such profile: {name} — add one with: mikan qbt set")
                }
                None => {
                    let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
                    bail!("no such profile: {name} — saved profiles: {}", names.join(", "))
                }
            };
            let client = connect_with_spinner(profile)?;
            println!("connected — qBittorrent {} at {}", client.version(), profile.endpoint);
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
        assert!(summary.contains("2 episodes · 1.63 GiB"), "was: {summary}");
        assert!(summary.contains("\n  • Anime - 01"), "was: {summary}");
        assert!(summary.contains("\n  • Anime - 02"), "was: {summary}");
    }

    #[test]
    fn episode_summary_is_singular_and_omits_unknown_size() {
        let mut episode = ep("Solo", None);
        episode.size = None;
        let row = Row(episode);
        let summary = episode_selection_summary(&[ListOption::new(0, &row)]);
        assert!(summary.contains("1 episode"), "was: {summary}");
        assert!(!summary.contains("episodes"), "should be singular: {summary}");
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
        assert!(hinted.contains("check your proxy"), "was: {hinted}");
    }

    #[test]
    fn proxy_hint_added_for_connect_errors() {
        let err = anyhow::anyhow!("error sending request: connection refused");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(hinted.contains("check your proxy"), "was: {hinted}");
    }

    #[test]
    fn no_proxy_hint_for_plain_http_404() {
        let err = anyhow::anyhow!("HTTP status client error (404 Not Found) for url (https://mikanani.me/RSS/x)");
        let hinted = format!("{:#}", add_proxy_hint(err));
        assert!(!hinted.contains("check your proxy"), "was: {hinted}");
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

        // Bare `mikan` parses (both None); run() turns it into a clap
        // usage error — covered by the smoke check.
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
            "specify at least one of --out, --url-list, --qbt"
        );

        let a = Args::try_parse_from(["mikan", "http://x/rss", "-y", "--qbt"]).unwrap();
        assert_eq!(headless_precondition(&a).unwrap_err(), "specify --all, --latest, or --filter");

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
}
