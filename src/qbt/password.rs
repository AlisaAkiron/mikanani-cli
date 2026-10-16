//! Interactive choice of where a profile's password lives (plain text, OS
//! keyring or 1Password), and `mikan qbt migrate` between them.

use anyhow::{Context, Result, bail};
use inquire::validator::Validation;
use inquire::{Password, PasswordDisplayMode, Text};
use std::fmt;
use std::path::Path;

use crate::config;
use crate::qbt::profile::lookup_profile;
use crate::qbt::secret;
use crate::ui::{cancelled, required, take_or_cancel};

/// How a profile's password is kept; see [`secret::resolve`].
#[derive(Clone, Copy, PartialEq)]
enum PasswordMethod {
    Plain,
    Keyring,
    OnePassword,
}

impl PasswordMethod {
    fn of(source: &secret::Source<'_>) -> PasswordMethod {
        match source {
            secret::Source::Plain(_) => PasswordMethod::Plain,
            secret::Source::Keyring(_) => PasswordMethod::Keyring,
            secret::Source::OnePassword(_) => PasswordMethod::OnePassword,
        }
    }

    fn name(self) -> &'static str {
        match self {
            PasswordMethod::Plain => "明文",
            PasswordMethod::Keyring => "系统密钥环",
            PasswordMethod::OnePassword => "1Password",
        }
    }
}

struct MethodOption {
    method: PasswordMethod,
    label: String,
}

impl fmt::Display for MethodOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// Default keyring entry key for a profile: `user@host:port`, so entries
/// stay recognisable in the OS keyring UI regardless of the profile name.
fn default_keyring_key(endpoint: &str, username: &str) -> String {
    let authority = reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_string();
            Some(match url.port_or_known_default() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| endpoint.to_string());
    format!("{username}@{authority}")
}

/// Ask how to keep a password. `exclude` drops one method from the menu
/// (the current one, when migrating); `default` places the cursor. A method
/// whose backend is unusable stays listed but marked, and picking it
/// explains why and asks again. Ok(None) on cancel.
fn select_password_method(
    default: Option<PasswordMethod>,
    exclude: Option<PasswordMethod>,
) -> Result<Option<PasswordMethod>> {
    let keyring_status = secret::keyring_status();
    let op_available = secret::onepassword_cli_available();
    let mut keyring_label = format!("系统密钥环 —— {}", secret::keyring_store_name());
    if keyring_status.is_err() {
        keyring_label.push_str("（当前不可用）");
    }
    let mut op_label = "1Password —— 通过 op CLI 读取".to_string();
    if !op_available {
        op_label.push_str("（未检测到 op 命令）");
    }
    let options: Vec<MethodOption> = [
        MethodOption { method: PasswordMethod::Plain, label: "明文 —— 保存在配置文件中".to_string() },
        MethodOption { method: PasswordMethod::Keyring, label: keyring_label },
        MethodOption { method: PasswordMethod::OnePassword, label: op_label },
    ]
    .into_iter()
    .filter(|o| Some(o.method) != exclude)
    .collect();
    // Without an explicit default, prefer the keyring when it is usable.
    let default = default.unwrap_or(if keyring_status.is_ok() {
        PasswordMethod::Keyring
    } else {
        PasswordMethod::Plain
    });
    let cursor = options.iter().position(|o| o.method == default).unwrap_or(0);

    loop {
        let Some(choice) = take_or_cancel(
            inquire::Select::new("密码保存方式：", options.iter().collect::<Vec<_>>())
                .with_starting_cursor(cursor)
                .prompt_skippable(),
            "密码保存方式",
        )? else {
            return Ok(None);
        };
        match choice.method {
            PasswordMethod::Keyring if keyring_status.is_err() => {
                let why = keyring_status.as_ref().unwrap_err();
                eprintln!("错误：无法打开{}：{why}", secret::keyring_store_name());
            }
            PasswordMethod::OnePassword if !op_available => {
                eprintln!("错误：未在 PATH 中找到 1Password CLI（op）—— 请先安装并登录");
            }
            method => return Ok(Some(method)),
        }
    }
}

fn prompt_secret(message: &str) -> Result<Option<String>> {
    take_or_cancel(
        Password::new(message)
            .without_confirmation()
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt_skippable(),
        "密码",
    )
}

/// Ask for a keyring entry key, prefilled with `default`. Ok(None) on cancel.
fn prompt_keyring_key(default: &str) -> Result<Option<String>> {
    let key = take_or_cancel(
        Text::new("密钥环条目名：")
            .with_initial_value(default)
            .with_validator(required("不能为空"))
            .prompt_skippable(),
        "密钥环条目名",
    )?;
    Ok(key.map(|k| k.trim().to_string()))
}

/// Ask for a 1Password secret reference (`op://…`), prefilled with
/// `initial`. Ok(None) on cancel.
fn prompt_onepassword_ref(initial: &str) -> Result<Option<String>> {
    let reference = take_or_cancel(
        Text::new("1Password 密码引用：")
            .with_initial_value(initial)
            .with_help_message("形如 op://<保险库>/<条目>/<字段>，可在 1Password 中右键字段 →「复制密码引用」")
            .with_validator(|input: &str| {
                let input = input.trim();
                if input.starts_with("op://") && input.len() > "op://".len() {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid("必须形如 op://<保险库>/<条目>/<字段>".into()))
                }
            })
            .prompt_skippable(),
        "1Password 密码引用",
    )?;
    Ok(reference.map(|r| r.trim().to_string()))
}

fn store_keyring_verbose(key: &str, password: &str) -> Result<()> {
    secret::store_keyring(key, password)?;
    println!("密码已保存到系统密钥环（{}/{key}）", secret::KEYRING_SERVICE);
    Ok(())
}

/// Ask how to keep the password, then collect it, returning the string
/// to store in the profile's `password` field (plain text, `keyring://…`
/// or `onepassword://…`). A keyring password is written to the OS store
/// right away so the connection test that follows can read it back.
/// Ok(None) on cancel.
pub fn prompt_password(
    existing: Option<&config::QbtProfile>,
    endpoint: &str,
    username: &str,
) -> Result<Option<String>> {
    let previous = existing.map(|p| secret::parse(&p.password));
    // Updating keeps the current method.
    let Some(method) = select_password_method(previous.as_ref().map(PasswordMethod::of), None)? else {
        return Ok(None);
    };
    match method {
        PasswordMethod::Plain => prompt_secret("密码："),
        PasswordMethod::Keyring => {
            let previous_key = match previous {
                Some(secret::Source::Keyring(key)) => Some(key),
                _ => None,
            };
            let default_key = previous_key
                .map(str::to_string)
                .unwrap_or_else(|| default_keyring_key(endpoint, username));
            let Some(key) = prompt_keyring_key(&default_key)? else {
                return Ok(None);
            };
            // Re-running setup on the same entry may keep its secret.
            let keep_hint = previous_key == Some(key.as_str());
            let message = if keep_hint { "密码（留空则保留密钥环中的现有密码）：" } else { "密码：" };
            let Some(password) = prompt_secret(message)? else {
                return Ok(None);
            };
            if !(keep_hint && password.is_empty()) {
                store_keyring_verbose(&key, &password)?;
            }
            Ok(Some(secret::keyring_ref(&key)))
        }
        PasswordMethod::OnePassword => {
            let initial = match previous {
                Some(secret::Source::OnePassword(reference)) => reference,
                _ => "op://",
            };
            let reference = prompt_onepassword_ref(initial)?;
            Ok(reference.map(|r| secret::onepassword_ref(&r)))
        }
    }
}

/// `mikan qbt migrate`: move a saved profile's password to another store.
/// The current password is read first and the new location is checked to
/// yield the same value before the config is rewritten, so a failed or
/// cancelled migration leaves the profile working as it was.
pub fn migrate_password(cfg: &mut config::Config, dir: &Path, name: &str) -> Result<i32> {
    let profile = lookup_profile(cfg, name)?.clone();
    if profile.username.is_empty() {
        println!("配置「{name}」使用本机免认证，没有需要迁移的密码");
        return Ok(0);
    }
    let current = secret::parse(&profile.password);
    let current_method = PasswordMethod::of(&current);
    let location = match current {
        secret::Source::Plain(_) => "配置文件".to_string(),
        secret::Source::Keyring(key) => format!("{}/{key}", secret::KEYRING_SERVICE),
        secret::Source::OnePassword(reference) => reference.to_string(),
    };
    println!("配置「{name}」当前使用：{}（{location}）", current_method.name());
    let password = secret::resolve(&profile.password).context("读取当前密码")?;

    let Some(target) = select_password_method(None, Some(current_method))? else {
        return Ok(cancelled());
    };
    let stored = match target {
        PasswordMethod::Plain => {
            let Some(true) = take_or_cancel(
                inquire::Confirm::new("密码将以明文保存在配置文件中，继续？")
                    .with_default(false)
                    .prompt_skippable(),
                "确认",
            )? else {
                return Ok(cancelled());
            };
            password.clone()
        }
        PasswordMethod::Keyring => {
            let Some(key) = prompt_keyring_key(&default_keyring_key(&profile.endpoint, &profile.username))? else {
                return Ok(cancelled());
            };
            store_keyring_verbose(&key, &password)?;
            secret::keyring_ref(&key)
        }
        PasswordMethod::OnePassword => {
            // mikan never writes into 1Password: the user stores the
            // password there, and we confirm the reference reads it back.
            println!("请先在 1Password 中保存该密码，然后填写其密码引用");
            let Some(reference) = prompt_onepassword_ref("op://")? else {
                return Ok(cancelled());
            };
            secret::onepassword_ref(&reference)
        }
    };
    if secret::resolve(&stored).context("读取迁移后的密码")? != password {
        bail!("新位置中的密码与当前密码不一致，配置未修改");
    }

    cfg.qbt.insert(name.to_string(), config::QbtProfile { password: stored, ..profile.clone() });
    cfg.save(dir).context("保存配置")?;
    println!("配置「{name}」的密码已迁移到{}", target.name());

    // The old copy: offer to delete a keyring entry; mikan leaves 1Password
    // items alone; a plain-text one is already gone from the rewritten file.
    match secret::parse(&profile.password) {
        secret::Source::Keyring(old_key) => {
            let still_used = cfg.qbt.values().any(|p| secret::parse(&p.password) == secret::Source::Keyring(old_key));
            if still_used {
                println!("旧的密钥环条目 {}/{old_key} 仍被其他配置使用，已保留", secret::KEYRING_SERVICE);
            } else if take_or_cancel(
                inquire::Confirm::new(&format!("删除旧的密钥环条目 {}/{old_key}？", secret::KEYRING_SERVICE))
                    .with_default(true)
                    .prompt_skippable(),
                "确认",
            )? == Some(true)
            {
                secret::delete_keyring(old_key)?;
                println!("已删除旧的密钥环条目");
            }
        }
        secret::Source::OnePassword(reference) => {
            println!("1Password 中的 {reference} 未作改动，如不再需要请自行删除");
        }
        secret::Source::Plain(_) => {}
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keyring_key_is_user_at_host_port() {
        assert_eq!(default_keyring_key("http://10.0.0.2:8080", "admin"), "admin@10.0.0.2:8080");
        // Implicit scheme ports are spelled out so http/https entries differ.
        assert_eq!(default_keyring_key("https://qbt.example.com", "me"), "me@qbt.example.com:443");
        assert_eq!(default_keyring_key("not a url", "me"), "me@not a url");
    }
}
