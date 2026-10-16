//! The per-episode execution loop shared by the wizard and headless mode.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::mikan::feed::Episode;
use crate::output::download::{self, Outcome};
use crate::output::url_list;
use crate::qbt::profile::QbtSetup;
use crate::sanitize;

/// Progress lines share a fixed-width status column so disk and qBittorrent
/// outcomes stay aligned even when interleaved. `tag` is the `[i/n]` counter.
fn progress(tag: &str, status: &str, detail: impl fmt::Display) {
    println!("{tag} {status:<16} {detail}");
}

/// Like [`progress`] but to stderr, for failures.
fn progress_err(tag: &str, status: &str, detail: impl fmt::Display) {
    eprintln!("{tag} {status:<16} {detail}");
}

/// Running tally the per-episode batch produces.
#[derive(Default)]
pub struct Totals {
    pub downloaded: u32,
    pub skipped: u32,
    pub failed: u32,
    pub qbt_added: u32,
    pub qbt_failed: u32,
}

impl Totals {
    /// Print the end-of-run summary lines for the outputs that were used.
    pub fn print_summary(&self, torrents: bool, qbt: bool) {
        if torrents {
            println!("已下载 {}，已跳过 {}，失败 {}", self.downloaded, self.skipped, self.failed);
        }
        if qbt {
            println!("已添加到 qBittorrent {}，失败 {}", self.qbt_added, self.qbt_failed);
        }
    }

    pub fn any_failure(&self) -> bool {
        self.failed > 0 || self.qbt_failed > 0
    }
}

/// Read a written/skipped .torrent back for upload; a read failure counts
/// as a qBt failure but never aborts the batch.
fn read_back_for_qbt(tag: &str, path: &Path, qbt_failed: &mut u32) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            *qbt_failed += 1;
            progress_err(tag, "失败", format!("回读 {}：{e}", path.display()));
            None
        }
    }
}

/// The per-episode batch. For each episode, optionally download the .torrent
/// to `torrents_dir` (when `Some`) and/or add it to qBittorrent (when
/// `qbt_setup` is `Some`), obtaining the bytes at most once and reusing them.
/// Prints one progress line per action; a per-episode failure is tallied and
/// never aborts the batch.
pub fn process_episodes(
    client: &reqwest::blocking::Client,
    episodes: &[Episode],
    torrents_dir: Option<&Path>,
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

/// Write the URL list and report the result. None means it failed (the
/// failure was already printed).
pub fn write_url_list(episodes: &[Episode], dir: &Path, channel_title: &str) -> Option<PathBuf> {
    match url_list::write_url_list(episodes, dir, channel_title) {
        Ok(path) => {
            println!("URL 列表已写入 {}", path.display());
            Some(path)
        }
        Err(e) => {
            eprintln!("失败           URL 列表：{e:#}");
            None
        }
    }
}
