//! Configuration parsing and validation.
//!
//! YAML schema: port, token/token_env, model_refresh_secs, upstreams
//! (name/kind/base_url/api_key_env/models/endpoint_by_model/surface_map_url),
//! mcp.servers (name/command/args/env/url/api_key_env). Upstream keys live in
//! env vars only, referenced by `api_key_env`.

use std::collections::HashMap;
use std::env as std_env;
use std::path::Path;
use crate::provider::ModelSurface;
use serde::Deserialize;
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
    pub name: String,
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
}

impl McpServerConfig {
    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|k| std_env::var(k).ok())
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One local embedding model served by a spawned llama-server child.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingModelConfig {
    /// Proxied id, exposed as `embeddings-local/<id>`. Must be unique.
    pub id: String,
    /// GGUF file path passed to llama-server (-m).
    pub model_file: String,
    /// Loopback port for the child; `None` = auto (18081 + index).
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_llama_bin() -> String {
    "llama-server".to_string()
}

fn default_idle_ttl() -> u64 {
    3600 // kill child after 1h with no traffic
}

/// Local embeddings: on-demand llama-server children behind the fake
/// `embeddings-local` provider.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingsConfig {
    pub llama_bin: String,
    pub idle_ttl_secs: u64,
    pub models: Vec<EmbeddingModelConfig>,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            llama_bin: default_llama_bin(),
            idle_ttl_secs: default_idle_ttl(),
            models: Vec::new(),
        }
    }
}

impl EmbeddingsConfig {
    /// Default loopback port for the model at `index` when none configured.
    pub fn port_for(&self, index: usize) -> u16 {
        self.models[index]
            .port
            .unwrap_or_else(|| 18081 + index as u16)
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
    let port: u16 = port.parse().map_err(|_| {
        ConfigError::Invalid(format!("bind port must be 0-65535, got '{port}'"))
    })?;
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
        let cfg: Config = serde_yaml::from_str(raw).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.bind_host_port()?;
        self.validate_upstreams()?;
        self.validate_mcp()?;
        self.validate_embeddings()?;
        if let Some(env) = &self.token_env {
            if std_env::var(env).is_err() {
                return Err(ConfigError::Invalid(format!(
                    "token_env '{env}' is not set in the environment"
                )));
            }
        }
        Ok(())
    }

    fn validate_upstreams(&self) -> Result<(), ConfigError> {
        let bad = |msg: String| Err(ConfigError::Invalid(msg));
        if self.upstreams.is_empty() {
            return bad("at least one upstream is required".into());
        }
        let mut seen = std::collections::HashSet::new();
        for u in &self.upstreams {
            if u.name.is_empty() {
                return bad("upstream name must not be empty".into());
            }
            if !seen.insert(u.name.as_str()) {
                return bad(format!("duplicate upstream name: {}", u.name));
            }
            for (model, surface) in &u.endpoint_by_model {
                if !matches!(surface.as_str(), "chat" | "messages" | "responses") {
                    return bad(format!(
                        "upstream '{}': endpoint_by_model['{model}'] must be one of chat|messages|responses, got '{surface}'",
                        u.name
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
                return bad(format!("mcp server '{}': command or url is required", s.name));
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
            if m.model_file.is_empty() {
                return bad(format!("embedding model '{}': model_file is required", m.id));
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
"#;

    #[test]
    fn defaults_applied() {
        let cfg = Config::from_yaml(MINIMAL).unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8080", "default bind = loopback:8080");
        assert_eq!(cfg.bind_host_port().unwrap(), ("127.0.0.1".to_string(), 8080));
        assert_eq!(cfg.model_refresh_secs, 0, "no auto-refresh by default");
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(cfg.upstreams[0].effective_base_url(), "https://api.openai.com/v1");
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn full_config_parses() {
        let cfg = Config::from_yaml(FULL).unwrap();
        assert_eq!(cfg.bind_host_port().unwrap(), ("127.0.0.1".to_string(), 9090));
        assert_eq!(cfg.effective_token(), Some("secret-literal".into()));
        assert_eq!(cfg.upstreams[0].models, vec!["gpt-4o", "gpt-4o-mini"]);
        assert!(!cfg.upstreams[0].discover, "discover defaults to false");
        assert_eq!(
            cfg.upstreams[1].effective_base_url(),
            "https://opencode.ai/zen/go/v1"
        );
        assert_eq!(
            cfg.upstreams[1].endpoint_by_model.get("qwen3.9-x").map(String::as_str),
            Some("messages")
        );
        assert_eq!(cfg.upstreams[1].surface_map_url.as_deref(), Some("https://opencode.ai/docs/go"));
        assert_eq!(
            cfg.upstreams[2].effective_base_url(),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(cfg.mcp.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(cfg.mcp.servers[1].url.as_deref(), Some("https://api.githubcopilot.com/mcp/"));
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
        assert_eq!(cfg.upstreams[1].subscription_token(), Some(Some("alice-tok".into())));
        assert_eq!(cfg.upstreams[2].subscription_token(), Some(None), "set-but-empty env = deny-all");
    }

    #[test]
    fn bind_parsing_cases() {
        let ok = Config::from_yaml("upstreams:\n  - { name: a, kind: openai }\n").unwrap();
        assert_eq!(ok.bind_host_port().unwrap(), ("127.0.0.1".to_string(), 8080));

        for (yaml, expect) in [
            ("bind: 0.0.0.0:9000", ("0.0.0.0".to_string(), 9000u16)),
            ("bind: 127.0.0.1:0", ("127.0.0.1".to_string(), 0u16)),
        ] {
            let cfg = Config::from_yaml(&format!("{yaml}\nupstreams:\n  - {{ name: a, kind: openai }}\n"))
                .unwrap();
            assert_eq!(cfg.bind_host_port().unwrap(), expect, "{yaml}");
        }

        for bad in ["bind: noslash", "bind: host:70000", "bind: :abc", "bind: :8081", "bind: "] {
            let cfg = Config::from_yaml(&format!("{bad}\nupstreams:\n  - {{ name: a, kind: openai }}\n"));
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
        assert_eq!(ok.upstreams[0].models, vec!["anthropic/claude-sonnet-4.5".to_string()]);
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
        assert_eq!(ok.upstreams[0].models, vec!["glm-5.3".to_string(), "glm-5.3-flash".to_string()]);
    }

    #[test]
    fn embeddings_block_defaults_and_models() {
        // no embeddings block -> defaults
        let cfg = Config::from_yaml("upstreams:\n  - { name: a, kind: openai }\n").unwrap();
        assert_eq!(cfg.embeddings.llama_bin, "llama-server");
        assert_eq!(cfg.embeddings.idle_ttl_secs, 3600);
        assert!(cfg.embeddings.models.is_empty());

        // full block
        let cfg = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  llama_bin: /opt/llama/bin/llama-server\n  idle_ttl_secs: 120\n  models:\n    - { id: nomic-embed-text-v1.5, model_file: /m/nomic.Q8_0.gguf, port: 18081 }\n    - { id: all-MiniLM-L6-v2, model_file: /m/minilm.Q8_0.gguf }\n",
        )
        .unwrap();
        assert_eq!(cfg.embeddings.llama_bin, "/opt/llama/bin/llama-server");
        assert_eq!(cfg.embeddings.idle_ttl_secs, 120);
        assert_eq!(cfg.embeddings.models.len(), 2);
        assert_eq!(cfg.embeddings.models[0].id, "nomic-embed-text-v1.5");
        assert_eq!(cfg.embeddings.models[0].model_file, "/m/nomic.Q8_0.gguf");
        assert_eq!(cfg.embeddings.models[0].port, Some(18081));
        assert_eq!(cfg.embeddings.models[1].port, None);
    }

    #[test]
    fn embeddings_validation_rejects_bad_config() {
        // duplicate model id
        let bad = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  models:\n    - { id: x, model_file: /m/x.gguf }\n    - { id: x, model_file: /m/y.gguf }\n",
        );
        assert!(bad.is_err(), "duplicate embedding id must be rejected");

        // port out of range (u16 overflow)
        let bad = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  models:\n    - { id: x, model_file: /m/x.gguf, port: 70000 }\n",
        );
        assert!(bad.is_err(), "port > 65535 must be rejected");

        // missing model_file
        let bad = Config::from_yaml(
            "upstreams:\n  - { name: a, kind: openai }\n\nembeddings:\n  models:\n    - { id: x }\n",
        );
        assert!(bad.is_err(), "model without model_file must be rejected");
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
    fn duplicate_upstream_names_rejected() {
        let yaml = r#"
upstreams:
  - { name: a, kind: openai }
  - { name: a, kind: anthropic }
"#;
        let err = Config::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn empty_upstreams_rejected() {
        let err = Config::from_yaml("upstreams: []\n").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn opencode_go_default_base_and_surface_url() {
        let cfg = Config::from_yaml("upstreams:\n  - { name: go, kind: opencode-go }\n").unwrap();
        assert_eq!(cfg.upstreams[0].effective_base_url(), "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.upstreams[0].surface_map_url_or_default(), "https://opencode.ai/docs/go");
    }

    #[test]
    fn invalid_endpoint_by_model_value_rejected() {
        let yaml = "upstreams:\n  - { name: go, kind: opencode-go, endpoint_by_model: { x: bogus } }\n";
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
}