use clap::Parser;
use std::path::PathBuf;

/// 一个用于下载 Mikan Project（蜜柑计划）番剧 RSS 订阅的交互式命令行工具。
#[derive(Parser)]
#[command(name = "mikan", version, about, args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Mikan RSS 订阅链接，例如："https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
    pub url: Option<String>,

    /// 代理地址（例如 http://127.0.0.1:7890）。默认使用代理环境变量（所有平台）以及 macOS/Windows 系统代理。
    #[arg(long)]
    pub proxy: Option<String>,

    /// 非交互模式运行：不显示任何提示，需要同时指定一个选择参数和一个输出参数
    #[arg(short = 'y', long, help_heading = "非交互模式")]
    pub yes: bool,

    /// 选择订阅中的所有剧集
    #[arg(long, help_heading = "非交互模式")]
    pub all: bool,

    /// 只保留最新的 N 集
    #[arg(long, value_name = "N", help_heading = "非交互模式")]
    pub latest: Option<usize>,

    /// 只保留标题包含 TEXT 的剧集（不区分大小写）
    #[arg(long, value_name = "TEXT", help_heading = "非交互模式")]
    pub filter: Option<String>,

    /// 将 .torrent 文件下载到 DIR
    #[arg(long, value_name = "DIR", help_heading = "非交互模式")]
    pub out: Option<PathBuf>,

    /// 将种子 URL 列表写入 DIR
    #[arg(long = "url-list", value_name = "DIR", help_heading = "非交互模式")]
    pub url_list: Option<PathBuf>,

    /// 使用已保存的配置添加到 qBittorrent（不带值时使用「default」；--qbt=NAME 指定配置名）
    #[arg(
        long,
        value_name = "PROFILE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "default",
        help_heading = "非交互模式"
    )]
    pub qbt: Option<String>,

    /// qBittorrent 分类（默认为清洗后的订阅标题）
    #[arg(long, value_name = "NAME", help_heading = "非交互模式")]
    pub category: Option<String>,
}

#[derive(clap::Subcommand)]
pub enum Command {
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
pub enum QbtAction {
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
    /// 将配置的密码迁移到另一种保存方式（明文 / 系统密钥环 / 1Password）
    Migrate {
        /// 要迁移的配置（默认为「default」）
        name: Option<String>,
    },
}

/// Emit `message` as a clap usage error (stderr + exit 2) and terminate.
pub fn usage_error(message: String) -> ! {
    use clap::CommandFactory;
    Args::command()
        .error(clap::error::ErrorKind::MissingRequiredArgument, message)
        .exit()
}

/// The first headless-only flag present, if any — used to reject headless
/// flags supplied without `-y`.
pub fn headless_flag_present(args: &Args) -> Option<&'static str> {
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
pub fn headless_precondition(args: &Args) -> Result<(), String> {
    if !(args.all || args.latest.is_some() || args.filter.is_some()) {
        return Err("请指定 --all、--latest 或 --filter".to_string());
    }
    if args.out.is_none() && args.url_list.is_none() && args.qbt.is_none() {
        return Err("请至少指定 --out、--url-list、--qbt 之一".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
