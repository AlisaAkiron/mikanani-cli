use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_HISTORY: usize = 10;

/// On-disk schema revision this build speaks. Bump it whenever the config
/// format changes in a backward-incompatible way; a file written by a newer
/// build (higher version) is refused rather than silently misread.
const CONFIG_VERSION: u32 = 1;

/// serde default for `Config::version`: a file missing the field predates
/// versioning entirely, so it is the first revision — v1.
fn default_version() -> u32 {
    1
}

/// Connection details for one qBittorrent WebUI. An empty username means
/// qBt's "bypass auth for localhost" mode (no login request is made).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct QbtProfile {
    pub endpoint: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

impl std::fmt::Debug for QbtProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QbtProfile")
            .field("endpoint", &self.endpoint)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Persistent app state. Every field carries #[serde(default)] so files
/// written by older versions keep loading as settings are added.
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Schema revision of this file. Missing in files predating versioning,
    /// which default to v1; see [`CONFIG_VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub path_history: Vec<String>,
    #[serde(default)]
    pub qbt: std::collections::BTreeMap<String, QbtProfile>,
}

impl Default for Config {
    fn default() -> Self {
        // A brand-new config is written at the current schema version.
        Config {
            version: CONFIG_VERSION,
            path_history: Vec::new(),
            qbt: std::collections::BTreeMap::new(),
        }
    }
}

impl Config {
    /// A missing or unreadable file yields defaults silently — state is a
    /// convenience and must not break a run. A file that is *present but
    /// unparseable* is moved aside to `config.toml.bak` (silently defaulting
    /// would let the next `save()` overwrite it and destroy saved qBittorrent
    /// profiles, passwords included), the user is told, and defaults returned.
    ///
    /// Loading fails only when the file declares a schema [`version`](Config::version)
    /// newer than [`CONFIG_VERSION`]: the file is left untouched and the caller
    /// aborts, so a config written by a future mikan is never silently misread.
    pub fn load(dir: &Path) -> anyhow::Result<Config> {
        let path = dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Config::default());
        };
        let cfg: Config = match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                let backup = dir.join("config.toml.bak");
                let rescued = std::fs::rename(&path, &backup).is_ok();
                let whereto = if rescued {
                    format!(" —— 原文件已移动到 {}", backup.display())
                } else {
                    String::new()
                };
                eprintln!(
                    "警告：{} 无法解析，已被忽略（{e}）{whereto}",
                    path.display()
                );
                return Ok(Config::default());
            }
        };
        if cfg.version > CONFIG_VERSION {
            anyhow::bail!(
                "配置文件 {} 的版本 {} 高于本程序支持的版本 {}，请升级 mikan 后再试",
                path.display(),
                cfg.version,
                CONFIG_VERSION
            );
        }
        Ok(cfg)
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let body = toml::to_string_pretty(self)?;
        let target = dir.join("config.toml");
        // The file holds qBt passwords: create the replacement with
        // owner-only permissions BEFORE writing any secret bytes, then
        // swap it into place atomically (also tightens a pre-existing
        // looser-mode file from older versions).
        let tmp = dir.join("config.toml.tmp");
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::remove_file(&tmp);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(body.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }

    pub fn record_path(&mut self, path: &str) {
        self.path_history.retain(|p| p != path);
        self.path_history.insert(0, path.to_string());
        self.path_history.truncate(MAX_HISTORY);
    }
}

pub fn config_dir() -> PathBuf {
    // Read every relevant variable up front; the resolver picks per platform.
    let appdata = std::env::var("APPDATA").ok();
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    resolve_config_dir(cfg!(windows), appdata.as_deref(), xdg.as_deref(), home.as_deref())
}

/// Resolve the config directory. `windows` selects the platform convention —
/// passed in (via `cfg!(windows)`) rather than read inside, so both branches
/// compile and stay unit-testable on any host:
///   - Windows: `%APPDATA%\mikanani-cli` (roaming app data).
///   - Linux/macOS: `$XDG_CONFIG_HOME/mikanani-cli`, else `$HOME/.config/mikanani-cli`.
///
/// Each branch falls back to the current directory when its variable is
/// missing, so a run never panics for want of an environment variable.
fn resolve_config_dir(
    windows: bool,
    appdata: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    if windows {
        let base = match appdata {
            Some(a) if !a.is_empty() => a,
            _ => ".",
        };
        return PathBuf::from(base).join("mikanani-cli");
    }
    match xdg {
        Some(x) if !x.is_empty() => PathBuf::from(x).join("mikanani-cli"),
        _ => PathBuf::from(home.unwrap_or(".")).join(".config").join("mikanani-cli"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_missing_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(cfg.path_history.is_empty());
    }

    #[test]
    fn load_corrupt_file_gives_default_and_backs_up_the_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "not [valid toml").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(cfg.path_history.is_empty());
        // The unparseable file must be preserved (not silently dropped, which
        // the next save() would clobber), and moved out of the load path.
        assert!(!dir.path().join("config.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            "not [valid toml"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.record_path("/a/b");
        cfg.record_path("/c/d");
        cfg.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.path_history, vec!["/c/d".to_string(), "/a/b".to_string()]);
    }

    #[test]
    fn save_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep").join("mikanani-cli");
        Config::default().save(&nested).unwrap();
        assert!(nested.join("config.toml").exists());
    }

    #[test]
    fn record_path_dedups_and_moves_to_front() {
        let mut cfg = Config::default();
        cfg.record_path("/a");
        cfg.record_path("/b");
        cfg.record_path("/a");
        assert_eq!(cfg.path_history, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn record_path_caps_at_ten() {
        let mut cfg = Config::default();
        for i in 0..12 {
            cfg.record_path(&format!("/p{i}"));
        }
        assert_eq!(cfg.path_history.len(), 10);
        assert_eq!(cfg.path_history[0], "/p11");
        assert_eq!(cfg.path_history[9], "/p2");
    }

    #[test]
    fn config_dir_unix_prefers_nonempty_xdg() {
        // Linux/macOS: XDG wins when set, else ~/.config; APPDATA is ignored.
        let cfg = PathBuf::from(".").join(".config").join("mikanani-cli");
        assert_eq!(resolve_config_dir(false, None, Some("/xdg"), Some("/home/u")), PathBuf::from("/xdg/mikanani-cli"));
        assert_eq!(
            resolve_config_dir(false, None, None, Some("/home/u")),
            PathBuf::from("/home/u/.config/mikanani-cli")
        );
        assert_eq!(
            resolve_config_dir(false, None, Some(""), Some("/home/u")),
            PathBuf::from("/home/u/.config/mikanani-cli")
        );
        assert_eq!(
            resolve_config_dir(false, Some(r"C:\AppData"), None, Some("/home/u")),
            PathBuf::from("/home/u/.config/mikanani-cli")
        );
        // No HOME either → current directory.
        assert_eq!(resolve_config_dir(false, None, None, None), cfg);
    }

    #[test]
    fn config_dir_windows_uses_appdata() {
        // Windows: %APPDATA%\mikanani-cli, ignoring XDG/HOME entirely.
        let appdata = r"C:\Users\u\AppData\Roaming";
        assert_eq!(resolve_config_dir(true, Some(appdata), None, None), PathBuf::from(appdata).join("mikanani-cli"));
        assert_eq!(
            resolve_config_dir(true, Some(appdata), Some("/xdg"), Some("/home/u")),
            PathBuf::from(appdata).join("mikanani-cli")
        );
        // Fallback to the current dir when APPDATA is missing or empty.
        assert_eq!(resolve_config_dir(true, None, None, None), PathBuf::from(".").join("mikanani-cli"));
        assert_eq!(resolve_config_dir(true, Some(""), None, None), PathBuf::from(".").join("mikanani-cli"));
    }

    #[test]
    fn qbt_profile_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.qbt.insert(
            "seedbox".to_string(),
            QbtProfile {
                endpoint: "http://10.0.0.2:8080".to_string(),
                username: "admin".to_string(),
                password: "secret".to_string(),
            },
        );
        cfg.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.qbt.get("seedbox"), cfg.qbt.get("seedbox"));
    }

    #[test]
    fn old_config_without_qbt_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "path_history = [\"/a\"]\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.path_history, vec!["/a".to_string()]);
        assert!(cfg.qbt.is_empty());
    }

    #[test]
    fn load_treats_missing_version_as_v1() {
        // Files written before the version field existed must load as v1,
        // regardless of what CONFIG_VERSION later becomes.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "path_history = [\"/a\"]\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn save_stamps_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            text.contains("version = 1"),
            "saved config should record its schema version, got:\n{text}"
        );
        assert_eq!(Config::load(dir.path()).unwrap().version, CONFIG_VERSION);
    }

    #[test]
    fn load_rejects_config_newer_than_supported() {
        // A file from a future build must abort the run rather than load and
        // let the next save() clobber fields this build doesn't understand.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let newer = format!("version = {}\npath_history = [\"/a\"]\n", CONFIG_VERSION + 1);
        std::fs::write(&path, &newer).unwrap();
        assert!(Config::load(dir.path()).is_err());
        // The too-new file must be left intact for a newer mikan to read.
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("config.toml")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_tightens_preexisting_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "path_history = []\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Config::default().save(dir.path()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
