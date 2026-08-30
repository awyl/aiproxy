//! Configuration parsing and validation.
//!
//! YAML schema: port, token/token_env, model_refresh_secs, upstreams
//! (name/kind/base_url/api_key_env/models/endpoint_by_model/surface_map_url),
//! mcp.servers (name/command/args/env/url/api_key_env). Upstream keys live in
//! env vars only, referenced by `api_key_env`.

use std::collections::HashMap;
use std::env as std_env;
use std::path::Path;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamKind {
    Openai,
    Anthropic,
    OpencodeGo,
}

impl UpstreamKind {
    pub fn default_base_url(self) -> &'static str {
        match self {
            UpstreamKind::Openai => "https://api.openai.com/v1",
            UpstreamKind::Anthropic => "https://api.anthropic.com/v1",
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

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_refresh")]
    pub model_refresh_secs: u64,
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub mcp: McpConfig,
}

fn default_port() -> u16 {
    8080
}
fn default_refresh() -> u64 {
    0 // fetch once at startup; set > 0 for periodic refresh
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
        let bad = |msg: String| Err(ConfigError::Invalid(msg));

        let mut seen = std::collections::HashSet::new();
        for u in &self.upstreams {
            if !seen.insert(u.name.as_str()) {
                return bad(format!("duplicate upstream name: {}", u.name));
            }
        }
        if self.upstreams.is_empty() {
            return bad("at least one upstream is required".into());
        }
        for u in &self.upstreams {
            if u.name.is_empty() {
                return bad("upstream name must not be empty".into());
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

        let mut seen = std::collections::HashSet::new();
        for s in &self.mcp.servers {
            if !seen.insert(s.name.as_str()) {
                return bad(format!("duplicate mcp server name: {}", s.name));
            }
        }
        for s in &self.mcp.servers {
            if s.name.is_empty() {
                return bad("mcp server name must not be empty".into());
            }
            if s.command.is_none() && s.url.is_none() {
                return bad(format!("mcp server '{}': command or url is required", s.name));
            }
        }

        if let Some(env) = &self.token_env {
            if std_env::var(env).is_err() {
                return bad(format!("token_env '{env}' is not set in the environment"));
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
port: 9090
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
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.model_refresh_secs, 0, "no auto-refresh by default");
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(cfg.upstreams[0].effective_base_url(), "https://api.openai.com/v1");
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn full_config_parses() {
        let cfg = Config::from_yaml(FULL).unwrap();
        assert_eq!(cfg.port, 9090);
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