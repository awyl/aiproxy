//! Configuration parsing and validation.
//!
//! YAML schema: port, token/token_env, model_refresh_secs, upstreams
//! (name/kind/base_url/api_key_env/models/endpoint_by_model/surface_map_url),
//! mcp.servers (name/command/args/env/url/api_key_env). Upstream keys live in
//! env vars only, referenced by `api_key_env`.

use crate::provider::ModelSurface;
use serde::Deserialize;
use std::collections::HashMap;
use std::env as std_env;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamKind {
    Openai,
    Anthropic,
    /// MiniMax (international) — OpenAI-compatible wire at
    /// api.minimax.io/v1. Token Plan / pay-as-you-go both use the same
    /// bearer key (MINIMAX_API_KEY).
    Minimax,
    /// Z.AI GLM Coding Plan — OpenAI-compatible wire at
    /// api.z.ai/api/coding/paas/v4 (chat). Anthropic Messages
    /// (/api/anthropic) and OpenAI Responses (/api/v1) also exist; the chat
    /// endpoint is the standard OpenAI-compatible one. Bearer key from the
    /// coding plan (individual or team; ZAI_API_KEY).
    Zai,
    /// OpenRouter — OpenAI-compatible aggregator at openrouter.ai/api/v1
    /// (chat). Model ids are `provider/model` (e.g. `anthropic/claude-sonnet-4.5`).
    /// The model catalog at GET /models is public (keyless); chat requests
    /// need the bearer key (OPENROUTER_API_KEY). Optional HTTP-Referer /
    /// X-Title attribution headers are not forwarded in v1.
    Openrouter,
    /// NVIDIA NIM cloud — OpenAI-compatible at integrate.api.nvidia.com/v1
    /// (chat). Model ids are `org/model` (e.g. `nvidia/llama-3.3-nemotron-super-49b-v1`).
    /// Catalog is public/keyless; chat needs the bearer key (NVIDIA_API_KEY,
    /// nvapi-...). Self-hosted NIMs use a custom base_url (kind still nvidia).
    Nvidia,
    OpencodeGo,
}

impl UpstreamKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Minimax => "minimax",
            Self::Zai => "zai",
            Self::Openrouter => "openrouter",
            Self::Nvidia => "nvidia",
            Self::OpencodeGo => "opencode-go",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            UpstreamKind::Openai => "https://api.openai.com/v1",
            UpstreamKind::Anthropic => "https://api.anthropic.com/v1",
            UpstreamKind::Minimax => "https://api.minimax.io/v1",
            UpstreamKind::Zai => "https://api.z.ai/api/coding/paas/v4",
            UpstreamKind::Openrouter => "https://openrouter.ai/api/v1",
            UpstreamKind::Nvidia => "https://integrate.api.nvidia.com/v1",
            UpstreamKind::OpencodeGo => "https://opencode.ai/zen/go/v1",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub name: Option<String>,
    pub kind: UpstreamKind,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Subscription token (env var name). When set, requests for THIS
    /// upstream must carry this token as Bearer — per-subscription routing.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Wire surface for static `models:` entries (chat|messages|responses).
    /// Falls back to the upstream kind's default (minimax -> chat). Static
    /// entries without a known surface are catalog-only (not streamable).
    #[serde(default)]
    pub surface: Option<ModelSurface>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub discover: bool,
    #[serde(default)]
    pub endpoint_by_model: HashMap<String, String>,
    #[serde(default)]
    pub surface_map_url: Option<String>,
}

impl UpstreamConfig {
    /// User-provided name or the kind name if omitted.
    pub fn effective_name(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| self.kind.as_str())
    }

    /// Provider ID used as the model prefix and subscription key.
    ///
    /// - Single upstream of kind: just the kind name (e.g. "opencode-go")
    /// - Multiple upstreams of kind: kind=name (e.g. "opencode-go=alice")
    ///
    /// Must be called after normalization (group_by_kind applied).
    pub fn provider_id(&self, count_in_kind: usize) -> String {
        if count_in_kind <= 1 {
            self.kind.as_str().to_string()
        } else {
            format!("{}={}", self.kind.as_str(), self.effective_name())
        }
    }

    pub fn surface_map_url_or_default(&self) -> String {
        self.surface_map_url
            .clone()
            .unwrap_or_else(|| "https://opencode.ai/docs/go".to_string())
    }

    pub fn effective_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.kind.default_base_url().to_string())
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|k| std_env::var(k).ok())
            .filter(|v| !v.is_empty())
    }

    /// Subscription token for this upstream, from `token_env`. `None` when
    /// the field is unset. When the env var is set but missing/empty, returns
    /// `Some(None)` — callers treat that as deny-all (misconfig).
    pub fn subscription_token(&self) -> Option<Option<String>> {
        let name = self.token_env.as_ref()?;
        Some(std_env::var(name).ok().filter(|v| !v.is_empty()))
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Per-server auth token (literal). When set, this server requires
    /// this token as Bearer — the global token is NOT accepted.
    #[serde(default)]
    pub token: Option<String>,
    /// Per-server auth token from an env var name. Takes precedence over
    /// the literal `token` field. When set but the env var is empty,
    /// falls back to the global token.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl McpServerConfig {
    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|k| std_env::var(k).ok())
            .filter(|v| !v.is_empty())
    }

    /// Per-server auth token: token_env > literal token > global fallback.
    pub fn effective_token(&self, global: &Option<String>) -> Option<String> {
        // 1. Per-server token from env var
        if let Some(env) = &self.token_env
            && let Some(val) = std_env::var(env).ok().filter(|v| !v.is_empty())
        {
            return Some(val);
        }
        // env var set but empty — fall through to global (not deny-all)
        // 2. Per-server literal token
        if let Some(t) = &self.token
            && !t.is_empty()
        {
            return Some(t.clone());
        }
        // 3. Global fallback
        global.clone()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// Hosts allowed by the streamable-HTTP server's DNS rebinding protection.
    /// Defaults to ["localhost", "127.0.0.1", "::1"] when empty; the bind
    /// host is always added automatically.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// One local embedding model served by the in-process fastembed backend.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingModelConfig {
    /// Proxied id, exposed as `embeddings-local/<id>`. Must be unique.
    pub id: String,
    /// fastembed `EmbeddingModel` variant name (e.g. "AllMiniLML6V2",
    /// "BGESmallENV15", "NomicEmbedTextV15"). Models are auto-downloaded
    /// from HuggingFace on first use.
    pub model: String,
    /// Optional output dimension override (some models support e.g. 256, 384,
    /// 512). `None` = model default.
    #[serde(default)]
    pub dimensions: Option<u32>,
}


fn default_idle_ttl() -> u64 {
    3600 // drop model after 1h with no traffic
}

/// Local embeddings: in-process fastembed backend behind the fake
/// `embeddings-local` provider. Models are loaded on demand and unloaded
/// by the idle reaper.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingsConfig {
    pub idle_ttl_secs: u64,
    pub models: Vec<EmbeddingModelConfig>,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            idle_ttl_secs: default_idle_ttl(),
            models: Vec::new(),
        }
    }
}

/// Listen address as "host:port". Defaults to loopback.
fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_refresh")]
    pub model_refresh_secs: u64,
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
}

impl Config {
    /// Split `bind` into (host, port).
    pub fn bind_host_port(&self) -> Result<(String, u16), ConfigError> {
        parse_bind(&self.bind)
    }
}
fn default_refresh() -> u64 {
    0 // fetch once at startup; set > 0 for periodic refresh
}

/// Parse "host:port" into (host, port). Port 0 (ephemeral) is allowed.
pub fn parse_bind(raw: &str) -> Result<(String, u16), ConfigError> {
    let Some((host, port)) = raw.rsplit_once(':') else {
        return Err(ConfigError::Invalid(format!(
            "bind must be 'host:port', got '{raw}'"
        )));
    };
    let port: u16 = port
        .parse()
        .map_err(|_| ConfigError::Invalid(format!("bind port must be 0-65535, got '{port}'")))?;
    if host.is_empty() {
        return Err(ConfigError::Invalid("bind host must not be empty".into()));
    }
    Ok((host.to_string(), port))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Config::from_yaml(&raw)
    }

    pub fn from_yaml(raw: &str) -> Result<Config, ConfigError> {
        let cfg: Config =
            serde_yaml::from_str(raw).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.bind_host_port()?;
        self.validate_upstreams()?;
        self.validate_mcp()?;
        self.validate_embeddings()?;
        if let Some(env) = &self.token_env
            && std_env::var(env).is_err()
        {
            return Err(ConfigError::Invalid(format!(
                "token_env '{env}' is not set in the environment"
            )));
        }
        Ok(())
    }

    fn validate_upstreams(&self) -> Result<(), ConfigError> {
        let bad = |msg: String| Err(ConfigError::Invalid(msg));
        if self.upstreams.is_empty() {
            return bad("at least one upstream is required".into());
        }
        // Group by kind for per-kind validation
        let mut by_kind: std::collections::HashMap<&str, Vec<&UpstreamConfig>> =
            std::collections::HashMap::new();
        for u in &self.upstreams {
            by_kind.entry(u.kind.as_str()).or_default().push(u);
        }
        for (kind, ups) in &by_kind {
            // Fail-fast: 2+ same kind must all have names
            if ups.len() > 1 {
                let missing_count = ups.iter().filter(|u| u.name.is_none()).count();
                if missing_count >= 2 {
                    let missing: Vec<String> = ups
                        .iter()
                        .filter(|u| u.name.is_none())
                        .map(|u| u.effective_name().to_string())
                        .collect();
                    return bad(format!(
                        "upstream kind '{kind}' has {} entries but {} are missing name (need unique names for 2+ same kind): {}",
                        ups.len(),
                        missing.len(),
                        missing.join(", ")
                    ));
                }
                // Check name uniqueness within kind
                let mut seen = std::collections::HashSet::new();
                for u in ups {
                    let name = u.effective_name();
                    if !seen.insert(name) {
                        return bad(format!(
                            "duplicate name '{name}' in kind '{kind}'"
                        ));
                    }
                }
            }
        }
        for u in &self.upstreams {
            for (model, surface) in &u.endpoint_by_model {
                if !matches!(surface.as_str(), "chat" | "messages" | "responses") {
                    return bad(format!(
                        "upstream '{}': endpoint_by_model['{model}'] must be one of chat|messages|responses, got '{surface}'",
                        u.effective_name()
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_mcp(&self) -> Result<(), ConfigError> {
        let bad = |msg: String| Err(ConfigError::Invalid(msg));
        let mut seen = std::collections::HashSet::new();
        for s in &self.mcp.servers {
            if s.name.is_empty() {
                return bad("mcp server name must not be empty".into());
            }
            if !seen.insert(s.name.as_str()) {
                return bad(format!("duplicate mcp server name: {}", s.name));
            }
            if s.command.is_none() && s.url.is_none() {
                return bad(format!(
                    "mcp server '{}': command or url is required",
                    s.name
                ));
            }
        }
        Ok(())
    }

    fn validate_embeddings(&self) -> Result<(), ConfigError> {
        let bad = |msg: String| Err(ConfigError::Invalid(msg));
        let mut seen = std::collections::HashSet::new();
        for m in &self.embeddings.models {
            if !seen.insert(m.id.as_str()) {
                return bad(format!("duplicate embedding model id: {}", m.id));
            }
            if m.model.is_empty() {
                return bad(format!(
                    "embedding model '{}': model variant is required",
                    m.id
                ));
            }
        }
        Ok(())
    }

    pub fn effective_token(&self) -> Option<String> {
        if let Some(env) = &self.token_env {
            return std_env::var(env).ok().filter(|v| !v.is_empty());
        }
        self.token.clone().filter(|v| !v.is_empty())
    }

    /// Compute provider IDs for all upstreams.
    /// - Single upstream of kind: ID = kind name (e.g. "opencode-go")
    /// - Multiple upstreams of kind: ID = kind=name (e.g. "opencode-go=alice")
    pub fn provider_ids(&self) -> Vec<String> {
        let mut by_kind: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for u in &self.upstreams {
            *by_kind.entry(u.kind.as_str()).or_insert(0) += 1;
        }
        self.upstreams
            .iter()
            .map(|u| {
                let count = by_kind[u.kind.as_str()];
                u.provider_id(count)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env mutation is unsafe in edition 2024; unique names + cleanup guard.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env_guarded(key: &str, value: &str) -> impl Drop {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var(key, value) };
        struct Guard(String);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var(&self.0) };
            }
        }
        Guard(key.to_string())
    }

    const MINIMAL: &str = r#"
upstreams:
  - name: openai
    kind: openai
"#;

    const FULL: &str = r#"
bind: 127.0.0.1:9090
token: secret-literal
model_refresh_secs: 60
upstreams:
  - name: openai
    kind: openai
    api_key_env: T_OPENAI_KEY
    models: [gpt-4o, gpt-4o-mini]
  - name: opencode-go
    kind: opencode-go
    api_key_env: T_GO_KEY
    endpoint_by_model:
      qwen3.9-x: messages
    surface_map_url: https://opencode.ai/docs/go
  - name: anthropic
    kind: anthropic
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
      env: { FOO: bar }
    - name: github
      url: https://api.githubcopilot.com/mcp/
      api_key_env: T_GITHUB_TOKEN
  allowed_hosts: ["host.containers.internal"]
"#;

    #[test]
    fn defaults_applied() {
        let cfg = Config::from_yaml(MINIMAL).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8080", "default bind = loopback:8080");
        assert_eq!(
            cfg.bind_host_port().unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );
        assert_eq!(cfg.model_refresh_secs, 0, "no auto-refresh by default");
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(
            cfg.upstreams[0].effective_base_url(),
            "https://api.openai.com/v1"
        );
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn full_config_parses() {
        let cfg = Config::from_yaml(FULL).unwrap();
        assert_eq!(
            cfg.bind_host_port().unwrap(),
            ("127.0.0.1".to_string(), 9090)
        );
        assert_eq!(cfg.effective_token(), Some("secret-literal".into()));
        assert_eq!(cfg.upstreams[0].models, vec!["gpt-4o", "gpt-4o-mini"]);
        assert!(!cfg.upstreams[0].discover, "discover defaults to false");
        assert_eq!(
            cfg.upstreams[1].effective_base_url(),
            "https://opencode.ai/zen/go/v1"
        );
        assert_eq!(
            cfg.upstreams[1]
                .endpoint_by_model
                .get("qwen3.9-x")
                .map(String::as_str),
            Some("messages")
        );
        assert_eq!(
            cfg.upstreams[1].surface_map_url.as_deref(),
            Some("https://opencode.ai/docs/go")
        );
        assert_eq!(
            cfg.upstreams[2].effective_base_url(),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(cfg.mcp.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(
            cfg.mcp.servers[1].url.as_deref(),
            Some("https://api.githubcopilot.com/mcp/")
        );
        assert_eq!(cfg.mcp.allowed_hosts, vec!["host.containers.internal"]);
    }

    #[test]
    fn subscription_token_reads_env() {
        let _g1 = set_env_guarded("T_SUB_ALICE", "alice-tok");
        let _g2 = set_env_guarded("T_SUB_UNSET", "");
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
  - { name: b, kind: openai, token_env: T_SUB_ALICE }
  - { name: c, kind: openai, token_env: T_SUB_UNSET  }
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        assert_eq!(cfg.upstreams[0].subscription_token(), None);
        assert_eq!(
            cfg.upstreams[1].subscription_token(),
            Some(Some("alice-tok".into()))
        );
        assert_eq!(
            cfg.upstreams[2].subscription_token(),
            Some(None),
            "set-but-empty env = deny-all"
        );
    }

    #[test]
    fn bind_parsing_cases() {
        let ok = Config::from_yaml("upstreams:\n  - { name: a, kind: openai }\n").unwrap();
        assert_eq!(
            ok.bind_host_port().unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );

        for (yaml, expect) in [
            ("bind: 0.0.0.0:9000", ("0.0.0.0".to_string(), 9000u16)),
            ("bind: 127.0.0.1:0", ("127.0.0.1".to_string(), 0u16)),
        ] {
            let cfg = Config::from_yaml(&format!(
                "{yaml}\nupstreams:\n  - {{ name: a, kind: openai }}\n"
            ))
            .unwrap();
            assert_eq!(cfg.bind_host_port().unwrap(), expect, "{yaml}");
        }

        for bad in [
            "bind: noslash",
            "bind: host:70000",
            "bind: :abc",
            "bind: :8081",
            "bind: ",
        ] {
            let cfg = Config::from_yaml(&format!(
                "{bad}\nupstreams:\n  - {{ name: a, kind: openai }}\n"
            ));
            assert!(cfg.is_err(), "expected reject: {bad}");
        }
    }

    #[test]
    fn nvidia_kind_defaults_to_chat_gateway_url() {
        assert_eq!(
            UpstreamKind::Nvidia.default_base_url(),
            "https://integrate.api.nvidia.com/v1"
        );

        let ok = Config::from_yaml(
            "upstreams:\n  - { name: nvidia, kind: nvidia, models: [nvidia/llama-3.3-nemotron-super-49b-v1] }\n",
        )
        .unwrap();
        assert_eq!(ok.upstreams[0].kind, UpstreamKind::Nvidia);
        assert_eq!(
            ok.upstreams[0].models,
            vec!["nvidia/llama-3.3-nemotron-super-49b-v1".to_string()]
        );
    }

    #[test]
    fn openrouter_kind_defaults_to_chat_gateway_url() {
        assert_eq!(
            UpstreamKind::Openrouter.default_base_url(),
            "https://openrouter.ai/api/v1"
        );

        let ok = Config::from_yaml(
            "upstreams:\n  - { name: openrouter, kind: openrouter, models: [anthropic/claude-sonnet-4.5] }\n",
        )
        .unwrap();
        assert_eq!(ok.upstreams[0].kind, UpstreamKind::Openrouter);
        assert_eq!(
            ok.upstreams[0].models,
            vec!["anthropic/claude-sonnet-4.5".to_string()]
        );
    }

    #[test]
    fn zai_kind_defaults_to_coding_plan_chat_gateway_url() {
        assert_eq!(
            UpstreamKind::Zai.default_base_url(),
            "https://api.z.ai/api/coding/paas/v4"
        );

        let ok = Config::from_yaml(
            "upstreams:\n  - { name: zai, kind: zai, models: [glm-5.3, glm-5.3-flash] }\n",
        )
        .unwrap();
        assert_eq!(ok.upstreams[0].kind, UpstreamKind::Zai);
        assert_eq!(
            ok.upstreams[0].models,
            vec!["glm-5.3".to_string(), "glm-5.3-flash".to_string()]
        );
    }

    #[test]
    fn embeddings_block_defaults_and_models() {
        // no embeddings block -> defaults
        let cfg = Config::from_yaml("upstreams:\n  - { name: a, kind: openai }\n").unwrap();
        assert_eq!(cfg.embeddings.idle_ttl_secs, 3600);
        assert!(cfg.embeddings.models.is_empty());

        // full block
        let cfg = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  idle_ttl_secs: 120\n  models:\n    - { id: nomic-embed-text-v1.5, model: NomicEmbedTextV15 }\n    - { id: all-MiniLM-L6-v2, model: AllMiniLML6V2, dimensions: 384 }\n",
        )
        .unwrap();
        assert_eq!(cfg.embeddings.idle_ttl_secs, 120);
        assert_eq!(cfg.embeddings.models.len(), 2);
        assert_eq!(cfg.embeddings.models[0].id, "nomic-embed-text-v1.5");
        assert_eq!(cfg.embeddings.models[0].model, "NomicEmbedTextV15");
        assert_eq!(cfg.embeddings.models[0].dimensions, None);
        assert_eq!(cfg.embeddings.models[1].model, "AllMiniLML6V2");
        assert_eq!(cfg.embeddings.models[1].dimensions, Some(384));
    }

    #[test]
    fn embeddings_validation_rejects_bad_config() {
        // duplicate model id
        let bad = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  models:\n    - { id: x, model: AllMiniLML6V2 }\n    - { id: x, model: AllMiniLML6V2 }\n",
        );
        assert!(bad.is_err(), "duplicate embedding id must be rejected");

        // missing model variant
        let bad = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  models:\n    - { id: x }\n",
        );
        assert!(bad.is_err(), "model without model variant must be rejected");
    }

    #[test]
    fn minimax_kind_defaults_to_anthropic_gateway_url() {
        assert_eq!(
            UpstreamKind::Minimax.default_base_url(),
            "https://api.minimax.io/v1"
        );

        let ok = Config::from_yaml(
            "upstreams:\n  - { name: minimax, kind: minimax, models: [MiniMax-M3] }\n",
        )
        .unwrap();
        assert_eq!(ok.upstreams[0].kind, UpstreamKind::Minimax);
        assert_eq!(ok.upstreams[0].models, vec!["MiniMax-M3".to_string()]);
    }

    #[test]
    fn duplicate_names_within_kind_rejected() {
        // Same kind, same name -> rejected
        let yaml = r#"
upstreams:
  - { name: alice, kind: openai }
  - { name: alice, kind: openai }
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn duplicate_names_across_kinds_allowed() {
        // Different kinds, same name -> allowed (name is per-kind)
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
  - { name: a, kind: anthropic }
"#;
        assert!(Config::from_yaml(yaml).is_ok());
    }

    #[test]
    fn fail_fast_on_missing_names_in_multi_kind() {
        // 2 upstreams of same kind, both missing name -> error
        let yaml = r#"
upstreams:
  - { kind: openai }
  - { kind: openai }
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("missing name"));
    }

    #[test]
    fn fail_fast_three_missing_names() {
        // 3 upstreams of same kind, all missing name -> error
        let yaml = r#"
upstreams:
  - { kind: openai }
  - { kind: openai }
  - { kind: openai }
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("missing name"));
    }

    #[test]
    fn multi_kind_one_named_one_unnamed_ok() {
        // 2 upstreams of same kind, one has name -> OK
        let yaml = r#"
upstreams:
  - { name: alice, kind: openai }
  - { kind: openai }
"#;
        assert!(Config::from_yaml(yaml).is_ok());
    }

    #[test]
    fn provider_ids_single_kind() {
        let cfg = Config::from_yaml(
            "upstreams:\n  - { kind: opencode-go }\n",
        )
        .unwrap();
        let ids = cfg.provider_ids();
        assert_eq!(ids, vec!["opencode-go"]);
    }

    #[test]
    fn provider_ids_multi_kind() {
        let cfg = Config::from_yaml(
            r#"
upstreams:
  - { name: alice, kind: opencode-go }
  - { name: bob, kind: opencode-go }
"#,
        )
        .unwrap();
        let ids = cfg.provider_ids();
        assert_eq!(ids, vec!["opencode-go=alice", "opencode-go=bob"]);
    }

    #[test]
    fn provider_ids_mixed_kinds() {
        let cfg = Config::from_yaml(
            r#"
upstreams:
  - { kind: opencode-go }
  - { name: alice, kind: openai }
  - { name: bob, kind: openai }
"#,
        )
        .unwrap();
        let ids = cfg.provider_ids();
        assert_eq!(ids, vec!["opencode-go", "openai=alice", "openai=bob"]);
    }

    #[test]
    fn empty_upstreams_rejected() {
        let err = Config::from_yaml("upstreams: []\n").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn opencode_go_default_base_and_surface_url() {
        let cfg = Config::from_yaml("upstreams:\n  - { name: go, kind: opencode-go }\n").unwrap();
        assert_eq!(
            cfg.upstreams[0].effective_base_url(),
            "https://opencode.ai/zen/go/v1"
        );
        assert_eq!(
            cfg.upstreams[0].surface_map_url_or_default(),
            "https://opencode.ai/docs/go"
        );
    }

    #[test]
    fn invalid_endpoint_by_model_value_rejected() {
        let yaml =
            "upstreams:\n  - { name: go, kind: opencode-go, endpoint_by_model: { x: bogus } }\n";
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("endpoint_by_model"));
    }

    #[test]
    fn discover_flag_parses() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai, discover: true }
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        assert!(cfg.upstreams[0].discover);
    }

    #[test]
    fn unknown_kind_rejected() {
        let yaml = r#"
upstreams:
  - { name: x, kind: alien }
"#;
        assert!(Config::from_yaml(yaml).is_err());
    }

    #[test]
    fn mcp_server_needs_command_or_url() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: broken
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("command or url"));
    }

    #[test]
    fn token_env_resolves_and_missing_env_is_error() {
        let _g = set_env_guarded("T_RUNTIME_TOKEN", "env-secret");
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
token_env: T_RUNTIME_TOKEN
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        assert_eq!(cfg.effective_token(), Some("env-secret".into()));

        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
token_env: T_DEFINITELY_UNSET_91283
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("token_env"));
    }

    #[test]
    fn api_key_reads_env_only() {
        let _g = set_env_guarded("T_UPSTREAM_KEY", "sk-test");
        let yaml = r#"
upstreams:
  - { name: a, kind: openai, api_key_env: T_UPSTREAM_KEY }
  - { name: b, kind: anthropic }
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        assert_eq!(cfg.upstreams[0].api_key(), Some("sk-test".into()));
        assert_eq!(cfg.upstreams[1].api_key(), None);
    }

    #[test]
    fn load_reads_file_and_reports_io_errors() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("aiproxy-test-{}.yaml", std::process::id()));
        std::fs::write(&path, MINIMAL).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.upstreams.len(), 1);
        std::fs::remove_file(&path).unwrap();

        let err = Config::load(&dir.join("definitely-missing-file-91283.yaml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)));
    }

    #[test]
    fn mcp_server_effective_token_fallback_to_global() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "mcp-fs"]
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let global = Some("global-tok".into());
        assert_eq!(
            cfg.mcp.servers[0].effective_token(&global),
            Some("global-tok".into()),
            "no per-server token -> use global"
        );
    }

    #[test]
    fn mcp_server_effective_token_literal_overrides_global() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "mcp-fs"]
      token: server-specific-tok
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let global = Some("global-tok".into());
        assert_eq!(
            cfg.mcp.servers[0].effective_token(&global),
            Some("server-specific-tok".into()),
            "literal token overrides global"
        );
    }

    #[test]
    fn mcp_server_effective_token_env_overrides_literal_and_global() {
        let _g = set_env_guarded("T_MCP_SERVER_TOK", "env-tok");
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "mcp-fs"]
      token: literal-tok
      token_env: T_MCP_SERVER_TOK
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let global = Some("global-tok".into());
        assert_eq!(
            cfg.mcp.servers[0].effective_token(&global),
            Some("env-tok".into()),
            "token_env overrides literal token"
        );
    }

    #[test]
    fn mcp_server_effective_token_empty_env_falls_back_to_global() {
        let _g = set_env_guarded("T_MCP_EMPTY", "");
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "mcp-fs"]
      token_env: T_MCP_EMPTY
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let global = Some("global-tok".into());
        assert_eq!(
            cfg.mcp.servers[0].effective_token(&global),
            Some("global-tok".into()),
            "empty token_env falls back to global"
        );
    }

    #[test]
    fn mcp_server_effective_token_no_global_no_per_server() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
mcp:
  servers:
    - name: fs
      command: npx
      args: ["-y", "mcp-fs"]
"#;
        let cfg = Config::from_yaml(yaml).unwrap();
        let global = None;
        assert_eq!(
            cfg.mcp.servers[0].effective_token(&global),
            None,
            "no per-server token + no global = no auth"
        );
    }
}
