mod cli;
mod commands;
mod config;
mod mikan;
mod net;
mod output;
mod qbt;
mod sanitize;
mod ui;

use anyhow::Result;
use clap::Parser;

use cli::{Args, Command, headless_flag_present, headless_precondition, usage_error};
use commands::{headless, search, wizard};
use qbt::profile;

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
        Some(Command::Qbt { action }) => return profile::qbt_command(action),
        Some(Command::Search { query, proxy }) => return search::search_flow(query, proxy.as_deref()),
        None => {}
    }
    let Some(url) = args.url.clone() else {
        // Bare `mikan` launches search — unless headless flags/-y were given,
        // which still require an explicit URL (preserve the old usage errors).
        if args.yes {
            usage_error("缺少必需的参数：\n  <URL>".to_string());
        }
        if let Some(flag) = headless_flag_present(&args) {
            usage_error(format!("{flag} 需要配合 --yes 使用（非交互模式）"));
        }
        return search::search_flow(None, args.proxy.as_deref());
    };
    if args.yes {
        if let Err(msg) = headless_precondition(&args) {
            usage_error(msg);
        }
        return headless::headless(&url, &args);
    }
    // Headless-only flags without -y: reject rather than silently open the
    // wizard when the user clearly meant a non-interactive run.
    if let Some(flag) = headless_flag_present(&args) {
        usage_error(format!("{flag} 需要配合 --yes 使用（非交互模式）"));
    }
    wizard::wizard(&url, args.proxy.as_deref())
}
