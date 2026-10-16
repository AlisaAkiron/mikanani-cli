use anyhow::{Context, bail};
use std::process::{Command, Stdio};

/// Keyring service name every `keyring://` entry lives under; the part after
/// the scheme is the entry's username/account.
pub const KEYRING_SERVICE: &str = "mikanani-cli";

const ONEPASSWORD_SCHEME: &str = "onepassword://";
const KEYRING_SCHEME: &str = "keyring://";

/// Where a profile's stored `password` string says the real secret lives.
#[derive(Debug, PartialEq)]
pub enum Source<'a> {
    Plain(&'a str),
    /// A 1Password secret reference (`op://vault/item/field`), read via `op`.
    OnePassword(&'a str),
    /// An entry key under [`KEYRING_SERVICE`] in the OS credential store.
    Keyring(&'a str),
}

pub fn parse(stored: &str) -> Source<'_> {
    if let Some(reference) = stored.strip_prefix(ONEPASSWORD_SCHEME) {
        Source::OnePassword(reference)
    } else if let Some(key) = stored.strip_prefix(KEYRING_SCHEME) {
        Source::Keyring(key)
    } else {
        Source::Plain(stored)
    }
}

/// Turn a profile's stored `password` into the actual secret:
///   - `onepassword://op://vault/item/field` → `op read op://vault/item/field`
///   - `keyring://<key>` → OS credential store entry (service [`KEYRING_SERVICE`], account `<key>`)
///   - anything else → used verbatim (plain text)
pub fn resolve(stored: &str) -> anyhow::Result<String> {
    match parse(stored) {
        Source::Plain(password) => Ok(password.to_string()),
        Source::OnePassword(reference) => read_onepassword(reference),
        Source::Keyring(key) => read_keyring(key),
    }
}

/// The stored form of a 1Password secret reference (`op://…`).
pub fn onepassword_ref(reference: &str) -> String {
    format!("{ONEPASSWORD_SCHEME}{reference}")
}

/// The stored form of a keyring entry key.
pub fn keyring_ref(key: &str) -> String {
    format!("{KEYRING_SCHEME}{key}")
}

/// Human name of the credential store `keyring://` uses on this OS.
pub fn keyring_store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS 钥匙串"
    } else if cfg!(windows) {
        "Windows 凭据管理器"
    } else {
        "Secret Service（GNOME Keyring / KWallet）"
    }
}

/// Whether this OS's credential store could be opened; Err carries why
/// (e.g. no D-Bus session on a headless Linux box).
pub fn keyring_status() -> Result<(), String> {
    keyring::Entry::store_status().as_ref().map(|_| ()).map_err(|e| e.to_string())
}

/// Whether the 1Password CLI (`op`) is runnable from PATH.
pub fn onepassword_cli_available() -> bool {
    Command::new("op")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Save `password` under keyring entry `key` (overwriting any existing one).
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub fn store_keyring(key: &str, password: &str) -> anyhow::Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, key)
        .and_then(|entry| entry.set_password(password))
        .with_context(|| format!("写入系统密钥环 {KEYRING_SERVICE}/{key} 失败"))
}

/// Save `password` under keyring entry `key` (overwriting any existing one).
///
/// Written through the Secret Service directly rather than the keyring
/// crate, whose items carry only `service`/`username`. KWallet's `ksecretd`
/// accepts those, but files them where the KWallet API that KWallet Manager
/// reads (`kwalletd6`) cannot list them; it places an item by KWallet's own
/// mapping — `server` = folder, `user` = entry, plus `type` — and only when
/// the item is created. Other providers just keep the extra attributes, and
/// reads still match on `service`/`username` through the keyring crate.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn store_keyring(key: &str, password: &str) -> anyhow::Result<()> {
    use secret_service::{EncryptionType, blocking::SecretService};
    use std::collections::HashMap;

    let write = || -> Result<(), secret_service::Error> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let collection = ss.get_default_collection()?;
        collection.ensure_unlocked()?;
        // Replace rather than add: an older item for this key (e.g. one
        // without the KWallet attributes) would make lookups ambiguous.
        // Searched service-wide, like reads, not just the default collection.
        let lookup = HashMap::from([("service", KEYRING_SERVICE), ("username", key)]);
        let found = ss.search_items(lookup)?;
        let locked: Vec<_> = found.locked.iter().collect();
        if !locked.is_empty() {
            ss.unlock_all(&locked)?;
        }
        for item in found.unlocked.iter().chain(&found.locked) {
            item.delete()?;
        }
        let attributes = HashMap::from([
            ("service", KEYRING_SERVICE),
            ("username", key),
            ("server", KEYRING_SERVICE),
            ("user", key),
            ("type", "plaintext"),
        ]);
        let label = format!("mikanani-cli qBittorrent 密码（{key}）");
        collection.create_item(&label, attributes, password.as_bytes(), true, "text/plain")?;
        Ok(())
    };
    write().with_context(|| format!("写入系统密钥环 {KEYRING_SERVICE}/{key} 失败"))
}

/// Delete keyring entry `key`; an already-missing entry is not an error.
pub fn delete_keyring(key: &str) -> anyhow::Result<()> {
    match keyring::Entry::new(KEYRING_SERVICE, key).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("删除系统密钥环条目 {KEYRING_SERVICE}/{key} 失败")),
    }
}

fn read_onepassword(reference: &str) -> anyhow::Result<String> {
    if !reference.starts_with("op://") {
        bail!("1Password 密码引用必须形如 onepassword://op://<保险库>/<条目>/<字段>");
    }
    // stdin/stderr inherited so `op` can prompt for sign-in / biometric
    // unlock and print its own errors; only stdout (the secret) is captured.
    let output = Command::new("op")
        .args(["read", "--no-newline", reference])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("运行 1Password CLI（op）失败 —— 是否已安装并在 PATH 中？")?;
    if !output.status.success() {
        bail!("1Password CLI 读取 {reference} 失败（{}）", output.status);
    }
    String::from_utf8(output.stdout).context("1Password 返回的密码不是有效的 UTF-8")
}

fn read_keyring(key: &str) -> anyhow::Result<String> {
    if key.is_empty() {
        bail!("keyring:// 后必须跟条目名");
    }
    keyring::Entry::new(KEYRING_SERVICE, key)
        .and_then(|entry| entry.get_password())
        .with_context(|| format!("从系统密钥环读取 {KEYRING_SERVICE}/{key} 失败"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_text() {
        assert_eq!(parse("hunter2"), Source::Plain("hunter2"));
        assert_eq!(parse(""), Source::Plain(""));
        // Other URL-looking strings are still just passwords.
        assert_eq!(parse("https://x"), Source::Plain("https://x"));
    }

    #[test]
    fn parse_onepassword_reference() {
        assert_eq!(
            parse("onepassword://op://Private/qBittorrent/password"),
            Source::OnePassword("op://Private/qBittorrent/password")
        );
    }

    #[test]
    fn parse_keyring_key() {
        assert_eq!(parse("keyring://seedbox"), Source::Keyring("seedbox"));
    }

    #[test]
    fn refs_round_trip_through_parse() {
        assert_eq!(parse(&keyring_ref("a@b:1")), Source::Keyring("a@b:1"));
        assert_eq!(parse(&onepassword_ref("op://V/I/f")), Source::OnePassword("op://V/I/f"));
    }

    #[test]
    fn resolve_plain_text_is_verbatim() {
        assert_eq!(resolve("hunter2").unwrap(), "hunter2");
    }

    #[test]
    fn resolve_rejects_malformed_references() {
        assert!(resolve("onepassword://Private/qBittorrent").is_err());
        assert!(resolve("keyring://").is_err());
    }
}
