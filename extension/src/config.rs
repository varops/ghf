//! Repository configuration (`.ghf`) and user configuration (trust,
//! credentials, machine id). Nothing secret ever lives in `.ghf`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ghf_proto::Policy;
use ghf_proto::retention::parse_duration_ms;
use serde::{Deserialize, Serialize};

use crate::error::{Context, Result};
use crate::{err, fail};

pub const FILE_NAME: &str = ".ghf";
pub const IGNORE_FILE: &str = ".ghfignore";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerConfig>,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub url: String,
    pub repository: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    /// `best-effort` (default) or `strict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `background` (default) or `manual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<String>,
    /// Files above this size are skipped with a record, e.g. `50MB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size: Option<String>,
    /// How long an idle writer keeps its lock, e.g. `30m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_ttl: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// Ref that finalized commits must be reachable from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_finalize: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfinalized_cap: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    BestEffort,
    Strict,
}

impl RepoConfig {
    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = root.join(FILE_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => fail!(Config, "cannot read {}: {e}", path.display()),
        };
        let cfg: RepoConfig =
            toml::from_str(&text).map_err(|e| err!(Config, "invalid {FILE_NAME}: {e}"))?;
        if cfg.version != 1 {
            fail!(Config, "unsupported {FILE_NAME} version {}", cfg.version);
        }
        cfg.validate()?;
        Ok(Some(cfg))
    }

    fn validate(&self) -> Result<()> {
        if let Some(s) = &self.server {
            normalize_url(&s.url)?;
            if !ghf_proto::api::valid_repo(&s.repository) {
                fail!(
                    Config,
                    "server.repository must look like owner/name, got {:?}",
                    s.repository
                );
            }
        }
        self.capture_mode()?;
        self.background_sync()?;
        self.max_file_size()?;
        self.writer_ttl_ms()?;
        self.policy()?;
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config serializes")
    }

    pub fn capture_mode(&self) -> Result<CaptureMode> {
        let env = std::env::var("GHF_STRICT").ok();
        if let Some(v) = env.as_deref() {
            return Ok(if matches!(v, "1" | "true" | "yes") {
                CaptureMode::Strict
            } else {
                CaptureMode::BestEffort
            });
        }
        match self.capture.mode.as_deref() {
            None | Some("best-effort") => Ok(CaptureMode::BestEffort),
            Some("strict") => Ok(CaptureMode::Strict),
            Some(o) => fail!(
                Config,
                "capture.mode must be best-effort or strict, got {o:?}"
            ),
        }
    }

    pub fn background_sync(&self) -> Result<bool> {
        let env = std::env::var("GHF_SYNC").ok();
        match env.as_deref().or(self.capture.sync.as_deref()) {
            None | Some("background") => Ok(true),
            Some("manual") => Ok(false),
            Some(o) => fail!(
                Config,
                "capture.sync must be background or manual, got {o:?}"
            ),
        }
    }

    pub fn max_file_size(&self) -> Result<u64> {
        match &self.capture.max_file_size {
            None => Ok(100 * 1024 * 1024),
            Some(s) => {
                parse_size(s).ok_or_else(|| err!(Config, "invalid capture.max_file_size {s:?}"))
            }
        }
    }

    pub fn writer_ttl_ms(&self) -> Result<i64> {
        match &self.capture.writer_ttl {
            None => Ok(30 * 60 * 1000),
            Some(s) => {
                parse_duration_ms(s).ok_or_else(|| err!(Config, "invalid capture.writer_ttl {s:?}"))
            }
        }
    }

    pub fn main_ref(&self) -> Option<&str> {
        self.git.as_ref().and_then(|g| g.main.as_deref())
    }

    /// Local retention policy, used when no server policy is known.
    pub fn policy(&self) -> Result<Policy> {
        let mut p = Policy::default();
        if let Some(r) = &self.retention {
            if let Some(s) = &r.after_finalize {
                p.after_finalize_ms = parse_duration_ms(s)
                    .ok_or_else(|| err!(Config, "invalid retention.after_finalize {s:?}"))?;
            }
            if let Some(s) = &r.unfinalized_cap {
                p.unfinalized_cap_ms = parse_duration_ms(s)
                    .ok_or_else(|| err!(Config, "invalid retention.unfinalized_cap {s:?}"))?;
            }
        }
        Ok(p)
    }
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// Canonical server URL: scheme and host, no trailing slash.
/// Plain HTTP is allowed only for loopback hosts.
pub fn normalize_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = if let Some(r) = u.strip_prefix("https://") {
        r
    } else if let Some(r) = u.strip_prefix("http://") {
        let host = match r.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => r.split(['/', ':']).next().unwrap_or_default(),
        };
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
        if !loopback && std::env::var("GHF_ALLOW_INSECURE").as_deref() != Ok("1") {
            fail!(
                Config,
                "server URL must use https (plain http is allowed only for localhost): {url}"
            );
        }
        r
    } else {
        fail!(Config, "server URL must start with https://: {url}");
    };
    if rest.is_empty() {
        fail!(Config, "server URL has no host: {url}");
    }
    Ok(u.to_owned())
}

// ---------------------------------------------------------------------------
// User configuration: ~/.config/ghf (or $GHF_CONFIG_DIR).

pub fn user_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("GHF_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME")
        && !d.is_empty()
    {
        return Ok(PathBuf::from(d).join("ghf"));
    }
    let home = std::env::var("HOME").map_err(|_| err!(Config, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config").join("ghf"))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .context(|| format!("write {}", tmp.display()))?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path).context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Stable random id for this machine's user account.
pub fn machine_id() -> Result<String> {
    let path = user_dir()?.join("machine-id");
    if let Ok(s) = fs::read_to_string(&path) {
        let s = s.trim();
        if ghf_proto::id::is_id(ghf_proto::id::MACHINE, s) {
            return Ok(s.to_owned());
        }
    }
    let id = ghf_proto::id::new_id(ghf_proto::id::MACHINE);
    write_private(&path, &format!("{id}\n"))?;
    Ok(id)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    servers: BTreeMap<String, TrustedServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedServer {
    pub server_id: String,
    pub trusted_at: i64,
}

fn trust_path() -> Result<PathBuf> {
    Ok(user_dir()?.join("trust.toml"))
}

fn read_toml<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(t) => toml::from_str(&t).map_err(|e| err!(Config, "invalid {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => fail!(Config, "cannot read {}: {e}", path.display()),
    }
}

pub fn trusted(url: &str) -> Result<Option<TrustedServer>> {
    let f: TrustFile = read_toml(&trust_path()?)?;
    Ok(f.servers.get(url).cloned())
}

pub fn trust(url: &str, server_id: &str) -> Result<()> {
    let path = trust_path()?;
    let mut f: TrustFile = read_toml(&path)?;
    f.servers.insert(
        url.to_owned(),
        TrustedServer {
            server_id: server_id.to_owned(),
            trusted_at: ghf_proto::id::now_ms(),
        },
    );
    write_private(&path, &toml::to_string(&f).expect("serialize trust"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    servers: BTreeMap<String, Credential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Credential {
    token: String,
}

fn credentials_path() -> Result<PathBuf> {
    Ok(user_dir()?.join("credentials.toml"))
}

/// Token for `url`: `$GHF_TOKEN`, else the user credentials file.
pub fn token(url: &str) -> Result<Option<String>> {
    if let Ok(t) = std::env::var("GHF_TOKEN")
        && !t.is_empty()
    {
        return Ok(Some(t));
    }
    let f: CredentialsFile = read_toml(&credentials_path()?)?;
    Ok(f.servers.get(url).map(|c| c.token.clone()))
}

pub fn save_token(url: &str, token: &str) -> Result<()> {
    let path = credentials_path()?;
    let mut f: CredentialsFile = read_toml(&path)?;
    f.servers.insert(
        url.to_owned(),
        Credential {
            token: token.to_owned(),
        },
    );
    write_private(&path, &toml::to_string(&f).expect("serialize credentials"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("50MB"), Some(50 * 1024 * 1024));
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("1x"), None);
    }

    #[test]
    fn urls() {
        assert!(normalize_url("https://ghf.example.com/").is_ok());
        assert!(normalize_url("http://127.0.0.1:7878").is_ok());
        assert!(normalize_url("ftp://x").is_err());
    }

    #[test]
    fn config_round_trip() {
        let c = RepoConfig {
            version: 1,
            server: Some(ServerConfig {
                url: "https://x".into(),
                repository: "a/b".into(),
            }),
            ..Default::default()
        };
        let back: RepoConfig = toml::from_str(&c.to_toml()).unwrap();
        assert_eq!(back.server.unwrap().repository, "a/b");
    }
}
