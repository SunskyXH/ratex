use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Wire-level protocol the endpoint speaks.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    OpenAi,
    Gemini,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Gemini => "gemini",
        }
    }
}

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
    pub protocol: Protocol,
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
#[derive(Debug, Default)]
pub struct ResolveInputs {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub concurrency: Option<usize>,
}

/// Forced XDG-style path on every platform: `$HOME/.config/ratex/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    Ok(PathBuf::from(home).join(".config/ratex/config.toml"))
}

/// Load when the file is optional (e.g. the default path). `Ok(None)` if missing.
pub fn load_optional(path: &Path) -> Result<Option<ConfigFile>> {
    match std::fs::read_to_string(path) {
        Ok(c) => Ok(Some(parse_str(path, &c)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read config {}", path.display())),
    }
}

/// Load when the user explicitly named the file (via `--config`). Missing is an error.
pub fn load_required(path: &Path) -> Result<ConfigFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config {}", path.display()))?;
    parse_str(path, &content)
}

fn parse_str(path: &Path, content: &str) -> Result<ConfigFile> {
    toml::from_str(content).with_context(|| format!("Failed to parse config {}", path.display()))
}

/// Merge config file + CLI flags. Precedence (high → low): CLI flags, profile fields, built-in defaults.
pub fn resolve(config: Option<&ConfigFile>, cli: ResolveInputs) -> Result<ResolvedProfile> {
    resolve_with_env(config, cli, |k| std::env::var(k).ok())
}

fn resolve_with_env<F>(
    config: Option<&ConfigFile>,
    cli: ResolveInputs,
    env: F,
) -> Result<ResolvedProfile>
where
    F: Fn(&str) -> Option<String>,
{
    // Pick a profile: CLI --profile, else config's default_profile.
    let profile_name = cli
        .profile
        .as_deref()
        .or_else(|| config.and_then(|c| c.default_profile.as_deref()));

    let Some(name) = profile_name else {
        bail!(
            "No profile selected. Pass --profile <name>, or set `default_profile = ...` in {}.",
            config_path_hint()
        );
    };
    let cfg = config.ok_or_else(|| {
        anyhow!(
            "Profile '{}' requested but no config file at {}.",
            name,
            config_path_hint()
        )
    })?;
    let profile = cfg.profiles.get(name).ok_or_else(|| {
        let mut available: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
        available.sort();
        anyhow!(
            "Profile '{}' not found. Available: [{}]",
            name,
            available.join(", ")
        )
    })?;

    let endpoint = cli
        .base_url
        .or_else(|| profile.endpoint.clone())
        .unwrap_or_else(|| default_endpoint(profile.protocol).to_string());
    let model = cli
        .model
        .or_else(|| profile.model.clone())
        .unwrap_or_else(|| default_model(profile.protocol).to_string());

    let api_key = if let Some(k) = cli.api_key {
        k
    } else {
        let env_var = profile
            .api_key_env
            .as_deref()
            .unwrap_or_else(|| default_api_key_env(profile.protocol));
        env(env_var).ok_or_else(|| {
            anyhow!(
                "API key not provided. Use --api-key or set {} environment variable.",
                env_var
            )
        })?
    };

    let concurrency = cli
        .concurrency
        .or(profile.concurrency)
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY);
    if concurrency == 0 {
        bail!("concurrency must be >= 1");
    }

    Ok(ResolvedProfile {
        protocol: profile.protocol,
        endpoint,
        model,
        api_key,
        concurrency,
    })
}

fn default_endpoint(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAi => "https://api.openai.com/v1",
        Protocol::Gemini => "https://generativelanguage.googleapis.com",
    }
}

fn default_model(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAi => "gpt-4o",
        Protocol::Gemini => "gemini-3-flash-preview",
    }
}

fn default_api_key_env(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAi => "OPENAI_API_KEY",
        Protocol::Gemini => "GEMINI_API_KEY",
    }
}

fn config_path_hint() -> String {
    default_config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.config/ratex/config.toml".into())
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
        protocol: Protocol,
        endpoint: Option<&str>,
        model: Option<&str>,
        api_key_env: Option<&str>,
    ) -> ProfileConfig {
        ProfileConfig {
            protocol,
            endpoint: endpoint.map(String::from),
            model: model.map(String::from),
            api_key_env: api_key_env.map(String::from),
            concurrency: None,
        }
    }

    fn pick(profile: &str) -> ResolveInputs {
        ResolveInputs {
            profile: Some(profile.into()),
            ..Default::default()
        }
    }

    #[test]
    fn default_profile_is_used_when_cli_omits_it() {
        let c = cfg(
            Some("claude"),
            vec![(
                "claude",
                profile(
                    Protocol::OpenAi,
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
    fn profile_falls_back_to_protocol_defaults() {
        let c = cfg(
            None,
            vec![("p", profile(Protocol::OpenAi, None, None, None))],
        );
        let env = env_map(&[("OPENAI_API_KEY", "fallback")]);
        let r = resolve_with_env(Some(&c), pick("p"), |k| env.get(k).cloned()).unwrap();
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
                    Protocol::OpenAi,
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
                profile: Some("p".into()),
                model: Some("cli-model".into()),
                base_url: Some("https://cli-endpoint".into()),
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
        let c = cfg(
            Some("p"),
            vec![("p", profile(Protocol::Gemini, None, None, None))],
        );
        let env = env_map(&[("GEMINI_API_KEY", "from-env")]);
        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                api_key: Some("from-cli".into()),
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
            vec![(
                "p",
                profile(Protocol::OpenAi, None, None, Some("CUSTOM_KEY")),
            )],
        );
        let env = env_map(&[("CUSTOM_KEY", "custom"), ("OPENAI_API_KEY", "default")]);
        let r = resolve_with_env(Some(&c), pick("p"), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.api_key, "custom");
    }

    #[test]
    fn no_profile_and_no_default_errs() {
        let c = cfg(None, vec![]);
        let err = resolve_with_env(Some(&c), ResolveInputs::default(), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("No profile selected"), "got: {err}");
    }

    #[test]
    fn profile_without_config_errs() {
        let err = resolve_with_env(None, pick("x"), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no config file"), "got: {err}");
    }

    #[test]
    fn profile_not_found_lists_available_sorted() {
        let c = cfg(
            None,
            vec![
                ("zeta", profile(Protocol::OpenAi, None, None, None)),
                ("alpha", profile(Protocol::Gemini, None, None, None)),
            ],
        );
        let err = resolve_with_env(Some(&c), pick("missing"), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Profile 'missing' not found"), "got: {err}");
        assert!(err.contains("alpha, zeta"), "got: {err}");
    }

    #[test]
    fn unknown_protocol_in_toml_errs_at_parse() {
        let toml_str = r#"
default_profile = "p"
[profiles.p]
protocol = "anthropic"
"#;
        let err = toml::from_str::<ConfigFile>(toml_str)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("anthropic") || err.contains("unknown variant"),
            "got: {err}"
        );
    }

    #[test]
    fn concurrency_resolution_cli_beats_profile_beats_global() {
        let mut c = cfg(
            None,
            vec![(
                "p",
                ProfileConfig {
                    protocol: Protocol::OpenAi,
                    endpoint: None,
                    model: None,
                    api_key_env: None,
                    concurrency: Some(6),
                },
            )],
        );
        c.concurrency = Some(2);
        let env = env_map(&[("OPENAI_API_KEY", "k")]);

        let r = resolve_with_env(Some(&c), pick("p"), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.concurrency, 6);

        let r = resolve_with_env(
            Some(&c),
            ResolveInputs {
                profile: Some("p".into()),
                concurrency: Some(10),
                ..Default::default()
            },
            |k| env.get(k).cloned(),
        )
        .unwrap();
        assert_eq!(r.concurrency, 10);
    }

    #[test]
    fn concurrency_falls_back_to_built_in_default() {
        let c = cfg(
            Some("p"),
            vec![("p", profile(Protocol::Gemini, None, None, None))],
        );
        let env = env_map(&[("GEMINI_API_KEY", "k")]);
        let r =
            resolve_with_env(Some(&c), ResolveInputs::default(), |k| env.get(k).cloned()).unwrap();
        assert_eq!(r.concurrency, DEFAULT_CONCURRENCY);
    }

    #[test]
    fn concurrency_zero_is_rejected() {
        let c = cfg(
            Some("p"),
            vec![("p", profile(Protocol::Gemini, None, None, None))],
        );
        let env = env_map(&[("GEMINI_API_KEY", "k")]);
        let err = resolve_with_env(
            Some(&c),
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
        let c = cfg(
            Some("p"),
            vec![("p", profile(Protocol::Gemini, None, None, None))],
        );
        let err = resolve_with_env(Some(&c), ResolveInputs::default(), |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GEMINI_API_KEY"), "got: {err}");
    }
}
