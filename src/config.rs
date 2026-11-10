use std::path::PathBuf;

use anyhow::{Result, bail};

/// Runtime configuration. All settings come from `LORE_*` environment variables.
/// Every later ticket adds its variables here, and to the table in docs/configuration.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: String,
    pub data_dir: PathBuf,
    pub admin_token: Option<String>,
    pub log: String,
    pub log_format: LogFormat,
    pub embedder: EmbedderKind,
    pub embed_model: String,
    /// `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS`: how long the first-start download of the embedding
    /// model may take, retries included, before the start fails.
    pub model_download_timeout_secs: u64,
    /// Largest accepted git bundle upload, in bytes.
    pub max_bundle_bytes: u64,
    pub version: String,
    /// Which model provider answers questions (`LORE_LLM`).
    pub llm: LlmKind,
    /// `ANTHROPIC_API_KEY`. Required when `llm` is `Anthropic`; checked by `llm::from_config`.
    pub anthropic_api_key: Option<String>,
    pub anthropic_base_url: String,
    pub fast_model: String,
    /// `None` when `LORE_FAST_EFFORT` is set to the empty string: no effort is sent.
    pub fast_effort: Option<String>,
    pub deep_model: String,
    /// `None` when `LORE_DEEP_EFFORT` is set to the empty string: no effort is sent.
    pub deep_effort: Option<String>,
    /// `LORE_OPENAI_BASE_URL`. Required when `llm` is `Openai`; checked by `llm::from_config`.
    pub openai_base_url: Option<String>,
    /// `LORE_OPENAI_MODEL`. Required when `llm` is `Openai`; checked by `llm::from_config`.
    pub openai_model: Option<String>,
    pub openai_api_key: Option<String>,
    /// `LORE_MCP_ALLOWED_HOSTS`: the `Host` values the MCP endpoint accepts. `None` (unset or
    /// empty) turns the host check off.
    pub mcp_allowed_hosts: Option<Vec<String>>,
    /// `LORE_SESSION_QUIET_SECS`: how long a session must have no new spans before it is
    /// turned into a searchable document. 0 means as soon as the worker runs.
    pub session_quiet_secs: u64,
    /// `LORE_DEEP_BUDGET_SECS`: the time budget of one deep-mode question, in seconds.
    pub deep_budget_secs: u64,
    /// `LORE_DEEP_MAX_ROUNDS`: the most tool rounds a deep-mode question may use.
    pub deep_max_rounds: u32,
    /// `LORE_DEEP_CONCURRENCY`: how many deep-mode questions one tenant may run at once.
    pub deep_concurrency: usize,
    /// `LORE_DEEP_DAILY_LIMIT`: how many deep-mode questions one tenant may ask per UTC day.
    pub deep_daily_limit: u32,
    /// `LORE_SYNC_INTERVAL_SECS`: how often every repo with a remote is fetched and indexed.
    /// `None` (unset or 0) turns the poller off.
    pub sync_interval_secs: Option<u64>,
    /// `LORE_GITHUB_WEBHOOK_SECRET`: the secret GitHub signs webhook deliveries with. `None`
    /// (unset or empty) turns the webhook endpoint off.
    pub github_webhook_secret: Option<String>,
    /// `LORE_SYNC_ALLOW_FILE_REMOTES`: accept `file://` remote URLs. For tests and local
    /// experiments only: it lets any ingest key make the server read a git repo on its host.
    pub sync_allow_file_remotes: bool,
    /// `LORE_EMBEDDING_CACHE`: who may reuse a vector another document already paid for.
    pub embedding_cache: EmbeddingCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmKind {
    Anthropic,
    Openai,
    Fake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Scope of the embedding cache (`LORE_EMBEDDING_CACHE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingCache {
    /// One cache for the whole server: a text embedded for any tenant is reused by every tenant.
    /// A push is faster when its text exists elsewhere, which is the only signal it gives away.
    Shared,
    /// One cache per tenant: no tenant's indexing time depends on another tenant's content.
    Tenant,
    /// No cache: only the chunks a document already holds keep their vectors.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    Fastembed,
    Hash,
}

/// The only embedding model v1 supports.
pub const EMBED_MODEL: &str = "bge-small-en-v1.5";

/// Default model for both answer modes.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// Default `ANTHROPIC_BASE_URL`.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Default `LORE_MODEL_DOWNLOAD_TIMEOUT_SECS`: 5 minutes.
pub const DEFAULT_MODEL_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Default `LORE_MAX_BUNDLE_BYTES`: 512 MiB.
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Default `LORE_SESSION_QUIET_SECS`.
pub const DEFAULT_SESSION_QUIET_SECS: u64 = 60;

/// Default `LORE_DEEP_BUDGET_SECS`.
pub const DEFAULT_DEEP_BUDGET_SECS: u64 = 30;

/// Default `LORE_DEEP_MAX_ROUNDS`.
pub const DEFAULT_DEEP_MAX_ROUNDS: u32 = 12;

/// Default `LORE_DEEP_CONCURRENCY`.
pub const DEFAULT_DEEP_CONCURRENCY: usize = 2;

/// Default `LORE_DEEP_DAILY_LIMIT`.
pub const DEFAULT_DEEP_DAILY_LIMIT: u32 = 100;

/// Reads a positive integer variable, or `default` when it is unset.
fn positive<T: std::str::FromStr + PartialOrd + Default>(
    f: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: T,
) -> Result<T> {
    match f(key) {
        None => Ok(default),
        Some(v) => match v.trim().parse::<T>() {
            Ok(n) if n > T::default() => Ok(n),
            _ => bail!("{key} must be a positive integer, got {v:?}"),
        },
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(f: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str, default: &str| f(key).unwrap_or_else(|| default.to_string());

        let log_format = match get("LORE_LOG_FORMAT", "text").as_str() {
            "text" => LogFormat::Text,
            "json" => LogFormat::Json,
            other => bail!("LORE_LOG_FORMAT must be \"text\" or \"json\", got {other:?}"),
        };

        let embedder = match get("LORE_EMBEDDER", "fastembed").as_str() {
            "fastembed" => EmbedderKind::Fastembed,
            "hash" => EmbedderKind::Hash,
            other => bail!("LORE_EMBEDDER must be \"fastembed\" or \"hash\", got {other:?}"),
        };
        let embedding_cache = match get("LORE_EMBEDDING_CACHE", "shared").as_str() {
            "shared" => EmbeddingCache::Shared,
            "tenant" => EmbeddingCache::Tenant,
            "off" => EmbeddingCache::Off,
            other => bail!(
                "LORE_EMBEDDING_CACHE must be \"shared\", \"tenant\" or \"off\", got {other:?}"
            ),
        };
        let embed_model = get("LORE_EMBED_MODEL", EMBED_MODEL);
        if embed_model != EMBED_MODEL {
            bail!(
                "LORE_EMBED_MODEL must be {EMBED_MODEL:?} (the only supported model), got {embed_model:?}"
            );
        }

        let model_download_timeout_secs = positive(
            &f,
            "LORE_MODEL_DOWNLOAD_TIMEOUT_SECS",
            DEFAULT_MODEL_DOWNLOAD_TIMEOUT_SECS,
        )?;

        let max_bundle_bytes = match f("LORE_MAX_BUNDLE_BYTES") {
            None => DEFAULT_MAX_BUNDLE_BYTES,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => bail!("LORE_MAX_BUNDLE_BYTES must be a positive integer, got {v:?}"),
            },
        };

        let session_quiet_secs = match f("LORE_SESSION_QUIET_SECS") {
            None => DEFAULT_SESSION_QUIET_SECS,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(n) => n,
                Err(_) => {
                    bail!("LORE_SESSION_QUIET_SECS must be a non-negative integer, got {v:?}")
                }
            },
        };

        let deep_budget_secs = positive(&f, "LORE_DEEP_BUDGET_SECS", DEFAULT_DEEP_BUDGET_SECS)?;
        let deep_max_rounds = positive(&f, "LORE_DEEP_MAX_ROUNDS", DEFAULT_DEEP_MAX_ROUNDS)?;
        let deep_concurrency = positive(&f, "LORE_DEEP_CONCURRENCY", DEFAULT_DEEP_CONCURRENCY)?;
        let deep_daily_limit = positive(&f, "LORE_DEEP_DAILY_LIMIT", DEFAULT_DEEP_DAILY_LIMIT)?;

        let sync_interval_secs = match f("LORE_SYNC_INTERVAL_SECS") {
            None => None,
            Some(v) => match v.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(n) => Some(n),
                Err(_) => {
                    bail!("LORE_SYNC_INTERVAL_SECS must be a non-negative integer, got {v:?}")
                }
            },
        };
        let sync_allow_file_remotes = match get("LORE_SYNC_ALLOW_FILE_REMOTES", "false").trim() {
            "true" | "1" => true,
            "false" | "0" | "" => false,
            other => bail!("LORE_SYNC_ALLOW_FILE_REMOTES must be true or false, got {other:?}"),
        };

        let llm = match get("LORE_LLM", "anthropic").as_str() {
            "anthropic" => LlmKind::Anthropic,
            "openai" => LlmKind::Openai,
            "fake" => LlmKind::Fake,
            other => bail!("LORE_LLM must be \"anthropic\", \"openai\" or \"fake\", got {other:?}"),
        };
        // An unset variable means the default, an empty one means "send no effort".
        let effort = |key: &str, default: &str| {
            let v = get(key, default);
            let v = v.trim();
            (!v.is_empty()).then(|| v.to_string())
        };
        // A set but empty value counts as unset.
        let non_empty = |key: &str| {
            f(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        Ok(Self {
            database_url: get(
                "LORE_DATABASE_URL",
                "postgres://lore:lore@localhost:5434/lore",
            ),
            listen_addr: get("LORE_LISTEN_ADDR", "0.0.0.0:8080"),
            data_dir: PathBuf::from(get("LORE_DATA_DIR", "./data")),
            admin_token: f("LORE_ADMIN_TOKEN").filter(|t| !t.is_empty()),
            log: get("LORE_LOG", "info"),
            log_format,
            embedder,
            embed_model,
            model_download_timeout_secs,
            max_bundle_bytes,
            version: get("LORE_VERSION", env!("CARGO_PKG_VERSION")),
            llm,
            anthropic_api_key: non_empty("ANTHROPIC_API_KEY"),
            anthropic_base_url: get("ANTHROPIC_BASE_URL", DEFAULT_ANTHROPIC_BASE_URL),
            fast_model: get("LORE_FAST_MODEL", DEFAULT_MODEL),
            fast_effort: effort("LORE_FAST_EFFORT", "low"),
            deep_model: get("LORE_DEEP_MODEL", DEFAULT_MODEL),
            deep_effort: effort("LORE_DEEP_EFFORT", "medium"),
            openai_base_url: non_empty("LORE_OPENAI_BASE_URL"),
            openai_model: non_empty("LORE_OPENAI_MODEL"),
            openai_api_key: non_empty("LORE_OPENAI_API_KEY"),
            mcp_allowed_hosts: parse_hosts(f("LORE_MCP_ALLOWED_HOSTS").as_deref()),
            session_quiet_secs,
            deep_budget_secs,
            deep_max_rounds,
            deep_concurrency,
            deep_daily_limit,
            sync_interval_secs,
            github_webhook_secret: f("LORE_GITHUB_WEBHOOK_SECRET").filter(|s| !s.is_empty()),
            sync_allow_file_remotes,
            embedding_cache,
        })
    }
}

/// Splits a comma separated host list. Blank entries are dropped, and a list with no entries
/// is `None`.
fn parse_hosts(raw: Option<&str>) -> Option<Vec<String>> {
    let hosts: Vec<String> = raw?
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    (!hosts.is_empty()).then_some(hosts)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = lookup(&[]).unwrap();
        assert_eq!(c.database_url, "postgres://lore:lore@localhost:5434/lore");
        assert_eq!(c.listen_addr, "0.0.0.0:8080");
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.admin_token, None);
        assert_eq!(c.log, "info");
        assert_eq!(c.log_format, LogFormat::Text);
        assert_eq!(c.embedder, EmbedderKind::Fastembed);
        assert_eq!(c.embed_model, "bge-small-en-v1.5");
        assert_eq!(c.model_download_timeout_secs, 300);
        assert_eq!(c.max_bundle_bytes, 512 * 1024 * 1024);
        assert_eq!(c.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(c.llm, LlmKind::Anthropic);
        assert_eq!(c.anthropic_api_key, None);
        assert_eq!(c.anthropic_base_url, "https://api.anthropic.com");
        assert_eq!(c.fast_model, "claude-opus-5-5");
        assert_eq!(c.fast_effort.as_deref(), Some("low"));
        assert_eq!(c.deep_model, "claude-opus-5-5");
        assert_eq!(c.deep_effort.as_deref(), Some("medium"));
        assert_eq!(c.openai_base_url, None);
        assert_eq!(c.openai_model, None);
        assert_eq!(c.openai_api_key, None);
        assert_eq!(c.mcp_allowed_hosts, None);
        assert_eq!(c.session_quiet_secs, 60);
        assert_eq!(c.deep_budget_secs, 30);
        assert_eq!(c.deep_max_rounds, 12);
        assert_eq!(c.deep_concurrency, 2);
        assert_eq!(c.deep_daily_limit, 100);
        assert_eq!(c.sync_interval_secs, None);
        assert_eq!(c.github_webhook_secret, None);
        assert!(!c.sync_allow_file_remotes);
        assert_eq!(c.embedding_cache, EmbeddingCache::Shared);
    }

    #[test]
    fn sync_settings_are_read() {
        let c = lookup(&[
            ("LORE_SYNC_INTERVAL_SECS", " 300 "),
            ("LORE_GITHUB_WEBHOOK_SECRET", "s3cret"),
            ("LORE_SYNC_ALLOW_FILE_REMOTES", "true"),
        ])
        .unwrap();
        assert_eq!(c.sync_interval_secs, Some(300));
        assert_eq!(c.github_webhook_secret.as_deref(), Some("s3cret"));
        assert!(c.sync_allow_file_remotes);
        let c = lookup(&[
            ("LORE_SYNC_INTERVAL_SECS", "0"),
            ("LORE_GITHUB_WEBHOOK_SECRET", ""),
        ])
        .unwrap();
        assert_eq!(c.sync_interval_secs, None);
        assert_eq!(c.github_webhook_secret, None);
        for (key, bad) in [
            ("LORE_SYNC_INTERVAL_SECS", "-1"),
            ("LORE_SYNC_INTERVAL_SECS", "soon"),
            ("LORE_SYNC_ALLOW_FILE_REMOTES", "yes"),
        ] {
            let err = lookup(&[(key, bad)]).unwrap_err();
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn deep_limits_are_read() {
        let c = lookup(&[
            ("LORE_DEEP_BUDGET_SECS", " 45 "),
            ("LORE_DEEP_MAX_ROUNDS", "3"),
            ("LORE_DEEP_CONCURRENCY", "4"),
            ("LORE_DEEP_DAILY_LIMIT", "7"),
        ])
        .unwrap();
        assert_eq!(c.deep_budget_secs, 45);
        assert_eq!(c.deep_max_rounds, 3);
        assert_eq!(c.deep_concurrency, 4);
        assert_eq!(c.deep_daily_limit, 7);
    }

    #[test]
    fn bad_deep_limits_are_an_error() {
        for key in [
            "LORE_DEEP_BUDGET_SECS",
            "LORE_DEEP_MAX_ROUNDS",
            "LORE_DEEP_CONCURRENCY",
            "LORE_DEEP_DAILY_LIMIT",
        ] {
            for bad in ["", "0", "-1", "lots", "1.5"] {
                let err = lookup(&[(key, bad)]).unwrap_err();
                assert!(err.to_string().contains(key), "{err}");
            }
        }
    }

    #[test]
    fn session_quiet_secs_is_read() {
        let c = lookup(&[("LORE_SESSION_QUIET_SECS", " 0 ")]).unwrap();
        assert_eq!(c.session_quiet_secs, 0);
        let c = lookup(&[("LORE_SESSION_QUIET_SECS", "300")]).unwrap();
        assert_eq!(c.session_quiet_secs, 300);
    }

    #[test]
    fn bad_session_quiet_secs_is_an_error() {
        for bad in ["", "-1", "soon", "1.5"] {
            let err = lookup(&[("LORE_SESSION_QUIET_SECS", bad)]).unwrap_err();
            assert!(err.to_string().contains("LORE_SESSION_QUIET_SECS"), "{err}");
        }
    }

    #[test]
    fn mcp_allowed_hosts_are_split_and_trimmed() {
        let c = lookup(&[(
            "LORE_MCP_ALLOWED_HOSTS",
            " lore.example.com, ,10.0.0.1:8080 ",
        )])
        .unwrap();
        assert_eq!(
            c.mcp_allowed_hosts,
            Some(vec![
                "lore.example.com".to_string(),
                "10.0.0.1:8080".to_string()
            ])
        );
        for empty in ["", " , "] {
            let c = lookup(&[("LORE_MCP_ALLOWED_HOSTS", empty)]).unwrap();
            assert_eq!(c.mcp_allowed_hosts, None);
        }
    }

    #[test]
    fn llm_settings_are_read() {
        let c = lookup(&[
            ("LORE_LLM", "openai"),
            ("ANTHROPIC_API_KEY", "test-key"),
            ("ANTHROPIC_BASE_URL", "http://localhost:1"),
            ("LORE_FAST_MODEL", "claude-haiku-4-5"),
            ("LORE_FAST_EFFORT", "high"),
            ("LORE_DEEP_MODEL", "claude-sonnet-5-5"),
            ("LORE_DEEP_EFFORT", "max"),
            ("LORE_OPENAI_BASE_URL", "http://localhost:8081/v1"),
            ("LORE_OPENAI_MODEL", "qwen"),
            ("LORE_OPENAI_API_KEY", "test-key"),
        ])
        .unwrap();
        assert_eq!(c.llm, LlmKind::Openai);
        assert_eq!(c.anthropic_api_key.as_deref(), Some("test-key"));
        assert_eq!(c.anthropic_base_url, "http://localhost:1");
        assert_eq!(c.fast_model, "claude-haiku-4-5");
        assert_eq!(c.fast_effort.as_deref(), Some("high"));
        assert_eq!(c.deep_model, "claude-sonnet-5-5");
        assert_eq!(c.deep_effort.as_deref(), Some("max"));
        assert_eq!(
            c.openai_base_url.as_deref(),
            Some("http://localhost:8081/v1")
        );
        assert_eq!(c.openai_model.as_deref(), Some("qwen"));
        assert_eq!(c.openai_api_key.as_deref(), Some("test-key"));
        assert_eq!(lookup(&[("LORE_LLM", "fake")]).unwrap().llm, LlmKind::Fake);
    }

    #[test]
    fn empty_effort_means_none() {
        let c = lookup(&[("LORE_FAST_EFFORT", ""), ("LORE_DEEP_EFFORT", "")]).unwrap();
        assert_eq!(c.fast_effort, None);
        assert_eq!(c.deep_effort, None);
    }

    #[test]
    fn bad_llm_is_an_error() {
        let err = lookup(&[("LORE_LLM", "gpt")]).unwrap_err();
        assert!(err.to_string().contains("LORE_LLM"), "{err}");
    }

    #[test]
    fn overrides_are_read() {
        let c = lookup(&[
            ("LORE_DATABASE_URL", "postgres://x/y"),
            ("LORE_LISTEN_ADDR", "127.0.0.1:9"),
            ("LORE_DATA_DIR", "/tmp/d"),
            ("LORE_ADMIN_TOKEN", "secret"),
            ("LORE_LOG", "debug"),
            ("LORE_LOG_FORMAT", "json"),
            ("LORE_EMBEDDER", "hash"),
            ("LORE_EMBED_MODEL", "bge-small-en-v1.5"),
            ("LORE_MAX_BUNDLE_BYTES", "1048576"),
            ("LORE_VERSION", "9.9.9"),
        ])
        .unwrap();
        assert_eq!(c.database_url, "postgres://x/y");
        assert_eq!(c.listen_addr, "127.0.0.1:9");
        assert_eq!(c.data_dir, PathBuf::from("/tmp/d"));
        assert_eq!(c.admin_token.as_deref(), Some("secret"));
        assert_eq!(c.log, "debug");
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(c.embedder, EmbedderKind::Hash);
        assert_eq!(c.max_bundle_bytes, 1_048_576);
        assert_eq!(c.version, "9.9.9");
    }

    #[test]
    fn empty_admin_token_is_none() {
        let c = lookup(&[("LORE_ADMIN_TOKEN", "")]).unwrap();
        assert_eq!(c.admin_token, None);
    }

    #[test]
    fn bad_log_format_is_an_error() {
        let err = lookup(&[("LORE_LOG_FORMAT", "yaml")]).unwrap_err();
        assert!(err.to_string().contains("LORE_LOG_FORMAT"), "{err}");
    }

    #[test]
    fn embedding_cache_is_read() {
        for (v, want) in [
            ("shared", EmbeddingCache::Shared),
            ("tenant", EmbeddingCache::Tenant),
            ("off", EmbeddingCache::Off),
        ] {
            let c = lookup(&[("LORE_EMBEDDING_CACHE", v)]).unwrap();
            assert_eq!(c.embedding_cache, want);
        }
        let err = lookup(&[("LORE_EMBEDDING_CACHE", "global")]).unwrap_err();
        assert!(err.to_string().contains("LORE_EMBEDDING_CACHE"), "{err}");
    }

    #[test]
    fn bad_embedder_is_an_error() {
        let err = lookup(&[("LORE_EMBEDDER", "openai")]).unwrap_err();
        assert!(err.to_string().contains("LORE_EMBEDDER"), "{err}");
    }

    #[test]
    fn unknown_embed_model_is_an_error() {
        let err = lookup(&[("LORE_EMBED_MODEL", "all-MiniLM-L6-v2")]).unwrap_err();
        assert!(err.to_string().contains("LORE_EMBED_MODEL"), "{err}");
    }

    #[test]
    fn model_download_timeout_is_read() {
        let c = lookup(&[("LORE_MODEL_DOWNLOAD_TIMEOUT_SECS", " 900 ")]).unwrap();
        assert_eq!(c.model_download_timeout_secs, 900);
    }

    #[test]
    fn bad_model_download_timeout_is_an_error() {
        for bad in ["", "0", "-1", "forever", "1.5"] {
            let err = lookup(&[("LORE_MODEL_DOWNLOAD_TIMEOUT_SECS", bad)]).unwrap_err();
            assert!(
                err.to_string().contains("LORE_MODEL_DOWNLOAD_TIMEOUT_SECS"),
                "{err}"
            );
        }
    }

    #[test]
    fn bad_max_bundle_bytes_is_an_error() {
        for bad in ["", "0", "-1", "big", "1.5"] {
            let err = lookup(&[("LORE_MAX_BUNDLE_BYTES", bad)]).unwrap_err();
            assert!(err.to_string().contains("LORE_MAX_BUNDLE_BYTES"), "{err}");
        }
    }
}
