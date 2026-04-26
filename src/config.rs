use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Wire-level protocol the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Gemini,
}

impl Protocol {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openai" => Ok(Protocol::OpenAi),
            "gemini" => Ok(Protocol::Gemini),
            other => bail!("Unknown protocol '{}'. Supported: openai, gemini.", other),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Gemini => "gemini",
        }
    }

    fn default_endpoint(self) -> &'static str {
        match self {
            Protocol::OpenAi => "https://api.openai.com/v1",
            Protocol::Gemini => "https://generativelanguage.googleapis.com",
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Protocol::OpenAi => "gpt-4o",
            Protocol::Gemini => "gemini-3-flash-preview",
        }
    }

    fn default_api_key_env(self) -> &'static str {
        match self {
            Protocol::OpenAi => "OPENAI_API_KEY",
            Protocol::Gemini => "GEMINI_API_KEY",
        }
    }
}

/// Built-in default for max concurrent translation requests.
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(Deserialize, Debug, Default)]
pub struct ConfigFile {
    pub default_profile: Option<String>,
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ProfileConfig {
    pub protocol: String,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub concurrency: Option<usize>,
}

#[derive(Debug)]
pub struct ResolvedProfile {
    pub protocol: Protocol,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub concurrency: usize,
}

/// CLI-supplied overrides fed into [`resolve`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveInputs<'a> {
    pub profile: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub concurrency: Option<usize>,
}

/// Forced XDG-style path on every platform: `$HOME/.config/ratex/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".config/ratex/config.toml"))
}

/// Load when the file is optional (e.g. the default path). `Ok(None)` if missing; `Err` only on read or parse failure.
pub fn load_optional(path: &Path) -> Result<Option<ConfigFile>> {
    match std::fs::read_to_string(path) {
        Ok(c) => Ok(Some(parse_str(path, &c)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read config {}", path.display())),
    }
}

/// Load when the user explicitly named the file (via `--config`). Missing file is an error.
pub fn load_required(path: &Path) -> Result<ConfigFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    parse_str(path, &content)
}

fn parse_str(path: &Path, content: &str) -> Result<ConfigFile> {
    toml::from_str(content).with_context(|| format!("Failed to parse config {}", path.display()))
}

/// Merge config file + CLI flags. Precedence (high → low): CLI flags, profile fields, built-in defaults.
pub fn resolve(config: Option<&ConfigFile>, cli: ResolveInputs<'_>) -> Result<ResolvedProfile> {
    resolve_with_env(config, cli, |k| std::env::var(k).ok())
}

fn resolve_with_env<F>(
    config: Option<&ConfigFile>,
    cli: ResolveInputs<'_>,
    env: F,
) -> Result<ResolvedProfile>
where
    F: Fn(&str) -> Option<String>,
{
    let base = match (cli.profile, cli.provider) {
        (Some(_), Some(_)) => bail!("--profile and --provider cannot both be set"),
        (Some(name), None) => {
            let cfg = config.ok_or_else(|| {
                anyhow!(
                    "--profile {} given but no config file at {}",
                    name,
                    default_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "~/.config/ratex/config.toml".into())
                )
            })?;
            cfg.profiles.get(name).cloned().ok_or_else(|| {
                let mut available: Vec<&String> = cfg.profiles.keys().collect();
                available.sort();
                anyhow!(
                    "Profile '{}' not found. Available: [{}]",
                    name,
                    available
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
        }
        (None, Some(provider)) => {
            eprintln!(
                "Warning: --provider is deprecated; prefer --profile or a config file at {}",
                default_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/.config/ratex/config.toml".into())
            );
            builtin_profile(provider)?
        }
        (None, None) => match config.and_then(|c| c.default_profile.as_deref()) {
            Some(name) => config
                .unwrap()
                .profiles
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("default_profile '{}' not found in [profiles]", name))?,
            None => builtin_profile("gemini")?,
        },
    };

    let protocol = Protocol::parse(&base.protocol)?;
    let endpoint = cli
        .base_url
        .map(String::from)
        .or(base.endpoint)
        .unwrap_or_else(|| protocol.default_endpoint().to_string());
    let model = cli
        .model
        .map(String::from)
        .or(base.model)
        .unwrap_or_else(|| protocol.default_model().to_string());

    let api_key = if let Some(k) = cli.api_key {
        k.to_string()
    } else {
        let env_var = base
            .api_key_env
            .as_deref()
            .unwrap_or_else(|| protocol.default_api_key_env());
        env(env_var).ok_or_else(|| {
            anyhow!(
                "API key not provided. Use --api-key or set {} environment variable.",
                env_var
            )
        })?
    };

    let concurrency = cli
        .concurrency
        .or(base.concurrency)
        .or(config.and_then(|c| c.concurrency))
        .unwrap_or(DEFAULT_CONCURRENCY);
    if concurrency == 0 {
        bail!("concurrency must be >= 1");
    }

    Ok(ResolvedProfile {
        protocol,
        endpoint,
        model,
        api_key,
        concurrency,
    })
}

fn builtin_profile(protocol: &str) -> Result<ProfileConfig> {
    let p = Protocol::parse(protocol)?;
    Ok(ProfileConfig {
        protocol: p.as_str().to_string(),
        endpoint: None,
        model: None,
        api_key_env: Some(p.default_api_key_env().to_string()),
        concurrency: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn cfg(default: Option<&str>, profiles: Vec<(&str, ProfileConfig)>) -> ConfigFile {
        ConfigFile {
            default_profile: default.map(String::from),
            concurrency: None,
            profiles: profiles
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn profile(
        protocol: &str,
        endpoint: Option<&str>,
        model: Option<&str>,
        api_key_env: Option<&str>,
    ) -> ProfileConfig {
        ProfileConfig {
            protocol: protocol.to_string(),
            endpoint: endpoint.map(String::from),
            model: model.map(String::from),
            api_key_env: api_key_env.map(String::from),
            concurrency: None,
        }
    }

    #[test]
    fn no_config_no_cli_uses_gemini_default() {
        let env = env_map(&[("GEMINI_API_KEY", "test-key")]);
        let r = resolve_with_env(None, ResolveInputs::default(), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.protocol, Protocol::Gemini);
        assert_eq!(r.model, "gemini-3-flash-preview");
        assert_eq!(r.endpoint, "https://generativelanguage.googleapis.com");
        assert_eq!(r.api_key, "test-key");
    }

    #[test]
    fn provider_flag_uses_builtin_openai() {
        let env = env_map(&[("OPENAI_API_KEY", "sk-test")]);
        let r = resolve_with_env(
            None,
            ResolveInputs {
                provider: Some("openai"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.protocol, Protocol::OpenAi);
        assert_eq!(r.model, "gpt-4o");
        assert_eq!(r.endpoint, "https://api.openai.com/v1");
        assert_eq!(r.api_key, "sk-test");
    }

    #[test]
    fn default_profile_is_used() {
        let c = cfg(
            Some("claude"),
            vec![(
                "claude",
                profile(
                    "openai",
                    Some("https://openrouter.ai/api/v1"),
                    Some("anthropic/claude-opus"),
                    Some("OPENROUTER_API_KEY"),
                ),
            )],
        );
        let env = env_map(&[("OPENROUTER_API_KEY", "sk-or")]);
        let r =
            resolve_with_env(Some(&c), ResolveInputs::default(), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.protocol, Protocol::OpenAi);
        assert_eq!(r.endpoint, "https://openrouter.ai/api/v1");
        assert_eq!(r.model, "anthropic/claude-opus");
        assert_eq!(r.api_key, "sk-or");
    }

    #[test]
    fn default_profile_pointing_to_missing_errs() {
        let c = cfg(Some("ghost"), vec![]);
        let err = resolve_with_env(Some(&c), ResolveInputs::default(), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default_profile 'ghost'"), "got: {err}");
    }

    #[test]
    fn profile_falls_back_to_protocol_defaults() {
        let c = cfg(None, vec![("p", profile("openai", None, None, None))]);
        let env = env_map(&[("OPENAI_API_KEY", "fallback")]);
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.endpoint, "https://api.openai.com/v1");
        assert_eq!(r.model, "gpt-4o");
        assert_eq!(r.api_key, "fallback");
    }

    #[test]
    fn cli_model_and_base_url_override_profile() {
        let c = cfg(
            None,
            vec![(
                "p",
                profile(
                    "openai",
                    Some("https://profile-endpoint"),
                    Some("profile-model"),
                    None,
                ),
            )],
        );
        let env = env_map(&[("OPENAI_API_KEY", "k")]);
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                model: Some("cli-model"),
                base_url: Some("https://cli-endpoint"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.model, "cli-model");
        assert_eq!(r.endpoint, "https://cli-endpoint");
    }

    #[test]
    fn cli_api_key_beats_profile_env_and_default_env() {
        let env = env_map(&[("GEMINI_API_KEY", "from-env")]);
        let r = resolve_with_env(
            None,
            ResolveInputs {
                api_key: Some("from-cli"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.api_key, "from-cli");
    }

    #[test]
    fn profile_api_key_env_beats_protocol_default_env() {
        let c = cfg(
            None,
            vec![("p", profile("openai", None, None, Some("CUSTOM_KEY")))],
        );
        let env = env_map(&[("CUSTOM_KEY", "custom"), ("OPENAI_API_KEY", "default")]);
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.api_key, "custom");
    }

    #[test]
    fn profile_and_provider_mutually_exclusive() {
        let err = resolve_with_env(
            None,
            ResolveInputs {
                profile: Some("a"),
                provider: Some("openai"),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot both be set"), "got: {err}");
    }

    #[test]
    fn profile_without_config_errs() {
        let err = resolve_with_env(
            None,
            ResolveInputs {
                profile: Some("x"),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no config file"), "got: {err}");
    }

    #[test]
    fn profile_not_found_lists_available_sorted() {
        let c = cfg(
            None,
            vec![
                ("zeta", profile("openai", None, None, None)),
                ("alpha", profile("gemini", None, None, None)),
            ],
        );
        let err = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("missing"),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Profile 'missing' not found"), "got: {err}");
        assert!(err.contains("alpha, zeta"), "got: {err}");
    }

    #[test]
    fn unknown_protocol_in_profile_errs() {
        let c = cfg(None, vec![("p", profile("anthropic", None, None, None))]);
        let err = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                ..Default::default()
            },
            |_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Unknown protocol 'anthropic'"), "got: {err}");
    }

    #[test]
    fn concurrency_defaults_to_built_in_when_unset() {
        let env = env_map(&[("GEMINI_API_KEY", "k")]);
        let r = resolve_with_env(None, ResolveInputs::default(), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.concurrency, DEFAULT_CONCURRENCY);
    }

    #[test]
    fn concurrency_resolution_cli_beats_profile_beats_global() {
        let mut c = cfg(
            None,
            vec![(
                "p",
                ProfileConfig {
                    protocol: "openai".into(),
                    endpoint: None,
                    model: None,
                    api_key_env: None,
                    concurrency: Some(6),
                },
            )],
        );
        c.concurrency = Some(2);
        let env = env_map(&[("OPENAI_API_KEY", "k")]);

        // Profile concurrency overrides global.
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.concurrency, 6);

        // CLI overrides profile.
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p"),
                concurrency: Some(10),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.concurrency, 10);
    }

    #[test]
    fn concurrency_zero_is_rejected() {
        let env = env_map(&[("GEMINI_API_KEY", "k")]);
        let err = resolve_with_env(
            None,
            ResolveInputs {
                concurrency: Some(0),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("concurrency"), "got: {err}");
    }

    #[test]
    fn missing_api_key_error_names_the_env_var() {
        let err = resolve_with_env(None, ResolveInputs::default(), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GEMINI_API_KEY"), "got: {err}");
    }
}
