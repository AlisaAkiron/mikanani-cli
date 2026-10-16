//! `mikan <URL> -y …`: the non-interactive run.

use anyhow::{Context, Result, bail};

use crate::cli::Args;
use crate::mikan::{feed, select};
use crate::output::batch;
use crate::qbt::profile::{self, QbtSetup};
use crate::{config, net};

/// Non-interactive counterpart to the wizard: no prompts, driven entirely by
/// flags. Preconditions (a selection flag and an output flag) are checked in
/// `run` before this is called. Reads qBittorrent profiles from config but
/// never writes config.
pub fn headless(url: &str, args: &Args) -> Result<i32> {
    let client = net::build_client(args.proxy.as_deref())?;
    let feed::Feed { channel_title, episodes } = net::fetch_feed(&client, url)?;
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
            let client = profile::connect(profile::lookup_profile(&cfg, profile_name)?)?;
            let category = args
                .category
                .clone()
                .unwrap_or_else(|| profile::default_category(&channel_title));
            client.ensure_category(&category)?;
            Some(QbtSetup { client, category, save_as: None })
        }
        None => None,
    };

    for dir in [args.out.as_ref(), args.url_list.as_ref()].into_iter().flatten() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建 {}", dir.display()))?;
    }

    let totals = batch::process_episodes(&client, &selected, args.out.as_deref(), qbt_setup.as_ref());
    totals.print_summary(args.out.is_some(), args.qbt.is_some());

    let url_failed = match &args.url_list {
        Some(dir) => batch::write_url_list(&selected, dir, &channel_title).is_none(),
        None => false,
    };

    Ok(if totals.any_failure() || url_failed { 1 } else { 0 })
}
