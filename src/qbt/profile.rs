//! qBittorrent connection profiles: the `mikan qbt …` subcommands and the
//! interactive prompts that create, pick and connect to a profile.

use anyhow::{Context, Result, bail};
use inquire::Text;
use inquire::validator::Validation;

use crate::cli::QbtAction;
use crate::qbt::password::{self, prompt_password};
use crate::ui::{Spinner, cancelled, required, take_or_cancel};
use crate::{config, qbt, sanitize};

/// Everything step 4 resolves: a connected client, the target category,
/// and — for an inline-created profile the user chose to save — its name.
pub struct QbtSetup {
    pub client: qbt::QbtClient,
    pub category: String,
    pub save_as: Option<(String, config::QbtProfile)>,
}

/// A connected client, plus the inline-created profile to save (if any).
type Connected = (qbt::QbtClient, Option<(String, config::QbtProfile)>);

pub fn qbt_command(action: QbtAction) -> Result<i32> {
    let dir = config::config_dir();
    let mut cfg = config::Config::load(&dir)?;
    match action {
        QbtAction::Set { name } => {
            let name = profile_name_or_default(name)?;
            let Some((_client, profile)) = prompt_and_connect(cfg.qbt.get(&name))? else {
                return Ok(cancelled());
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
            let profile = lookup_profile(&cfg, &name)?;
            let client = connect_with_spinner(profile)?;
            println!("已连接 —— qBittorrent {} @ {}", client.version(), profile.endpoint);
            Ok(0)
        }
        QbtAction::Migrate { name } => {
            let name = profile_name_or_default(name)?;
            password::migrate_password(&mut cfg, &dir, &name)
        }
    }
}

/// A saved profile by name, or an error listing the ones that exist.
pub fn lookup_profile<'a>(cfg: &'a config::Config, name: &str) -> Result<&'a config::QbtProfile> {
    match cfg.qbt.get(name) {
        Some(profile) => Ok(profile),
        None if cfg.qbt.is_empty() => bail!("没有名为「{name}」的配置 —— 使用 mikan qbt set 添加"),
        None => {
            let names: Vec<&str> = cfg.qbt.keys().map(String::as_str).collect();
            bail!("没有名为「{name}」的配置 —— 已保存的配置：{}", names.join(", "))
        }
    }
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

/// Default qBt category for an import: the sanitized feed title.
pub fn default_category(channel_title: &str) -> String {
    let stem = sanitize::sanitize_stem(channel_title);
    if stem.is_empty() { "mikan".to_string() } else { stem }
}

/// Connect to qBittorrent, showing a spinner during the blocking attempt.
fn connect_with_spinner(profile: &config::QbtProfile) -> Result<qbt::QbtClient> {
    let _spinner = Spinner::start("正在连接 qBittorrent…");
    qbt::QbtClient::connect(profile)
}

/// [`connect_with_spinner`], announcing the server version on success.
pub fn connect(profile: &config::QbtProfile) -> Result<qbt::QbtClient> {
    let client = connect_with_spinner(profile)?;
    println!("已连接 —— qBittorrent {}", client.version());
    Ok(client)
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
        match connect(&profile) {
            Ok(client) => return Ok(Some((client, profile))),
            Err(e) => {
                eprintln!("错误：{e:#}");
                prefill = Some(profile);
            }
        }
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
    let endpoint = endpoint.trim().trim_end_matches('/').to_string();
    let username = username.trim().to_string();
    let password = if username.is_empty() {
        String::new()
    } else {
        let Some(password) = prompt_password(existing, &endpoint, &username)? else {
            return Ok(None);
        };
        password
    };
    Ok(Some(config::QbtProfile { endpoint, username, password }))
}

/// Wizard step 4: resolve profile (inline creation when none saved),
/// connect, and ask the category. Ok(None) = user cancelled.
pub fn resolve_qbt(cfg: &config::Config, channel_title: &str) -> Result<Option<QbtSetup>> {
    let connected = if cfg.qbt.is_empty() {
        create_profile_inline()?
    } else {
        connect_saved_profile(cfg)?
    };
    let Some((client, save_as)) = connected else {
        return Ok(None);
    };

    let Some(category) = take_or_cancel(
        Text::new("qBittorrent 分类（分组/文件夹）：")
            .with_initial_value(&default_category(channel_title))
            .with_validator(required("分类不能为空"))
            .prompt_skippable(),
        "分类",
    )? else {
        return Ok(None);
    };

    Ok(Some(QbtSetup { client, category: category.trim().to_string(), save_as }))
}

/// No saved profiles: create one, connect, and offer to save it (the save
/// itself happens later, once the wizard executes).
fn create_profile_inline() -> Result<Option<Connected>> {
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
    if !save {
        return Ok(Some((client, None)));
    }
    let Some(name) = take_or_cancel(
        Text::new("配置名称：")
            .with_initial_value("default")
            .with_validator(required("名称不能为空"))
            .prompt_skippable(),
        "配置名称",
    )? else {
        return Ok(None);
    };
    Ok(Some((client, Some((name.trim().to_string(), profile)))))
}

/// One or more saved profiles: pick one (auto when there's only one),
/// connect, and on failure let the user retry or switch — never abort the
/// whole wizard and discard the episode/format/path choices.
fn connect_saved_profile(cfg: &config::Config) -> Result<Option<Connected>> {
    loop {
        let profile = if cfg.qbt.len() == 1 {
            let (name, profile) = cfg.qbt.iter().next().expect("len checked");
            println!("使用 qBittorrent 配置「{name}」");
            profile
        } else {
            let names: Vec<String> = cfg.qbt.keys().cloned().collect();
            let Some(name) = take_or_cancel(
                inquire::Select::new("qBittorrent 配置：", names).prompt_skippable(),
                "qBittorrent 配置",
            )? else {
                return Ok(None);
            };
            &cfg.qbt[&name]
        };
        match connect(profile) {
            Ok(client) => return Ok(Some((client, None))),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_category_sanitizes_and_falls_back() {
        assert_eq!(default_category("Mikan Project - 石纪元"), "Mikan Project - 石纪元");
        assert_eq!(default_category("A / B"), "A ⁄ B");
        assert_eq!(default_category("..."), "mikan");
    }
}
