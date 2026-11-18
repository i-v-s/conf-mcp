use anyhow::{Context, Result, bail};
use reqwest::header::HeaderValue;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use url::Url;

pub struct Config {
    pub base_url: Url,
    pub authorization: HeaderValue,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    base_url: Option<String>,
    token: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = env::var_os("HOME").map(|home| Path::new(&home).join(".config/conf-mcp.toml"));
        Self::from_sources(
            path.as_deref(),
            read_env("CONFLUENCE_BASE_URL")?,
            read_env("CONFLUENCE_TOKEN")?,
        )
    }

    fn from_sources(
        path: Option<&Path>,
        env_url: Option<String>,
        env_token: Option<String>,
    ) -> Result<Self> {
        let file = match path.map(fs::read_to_string) {
            Some(Ok(text)) => toml::from_str::<FileConfig>(&text)
                .map_err(|_| anyhow::anyhow!("Некорректный TOML в ~/.config/conf-mcp.toml"))?,
            Some(Err(error)) if error.kind() != ErrorKind::NotFound => {
                bail!(
                    "Не удалось прочитать ~/.config/conf-mcp.toml: {}",
                    error.kind()
                )
            }
            _ => FileConfig::default(),
        };
        let url = env_url
            .or(file.base_url)
            .context("Укажите base_url в ~/.config/conf-mcp.toml или CONFLUENCE_BASE_URL")?;
        let token = env_token
            .or(file.token)
            .context("Укажите token в ~/.config/conf-mcp.toml или CONFLUENCE_TOKEN")?;
        Self::new(&url, &token)
    }

    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let mut base_url = Url::parse(base_url.trim())
            .context("base_url должен быть абсолютным HTTP(S)-адресом Confluence")?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            bail!("base_url должен быть HTTP(S)-адресом без логина, пароля, query и fragment");
        }
        let path = format!("{}/", base_url.path().trim_end_matches('/'));
        base_url.set_path(&path);
        if token.trim().is_empty() || token.chars().any(char::is_whitespace) {
            bail!(
                "Токен Confluence должен быть непустым и не содержать пробелов или переводов строк"
            );
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("Токен Confluence содержит недопустимые символы HTTP-заголовка")?;
        authorization.set_sensitive(true);
        Ok(Self {
            base_url,
            authorization,
        })
    }
}

fn read_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("Переменная {name} должна содержать UTF-8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_and_environment_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "base_url = 'https://wiki.test/confluence'\ntoken = 'file-secret'",
        )
        .unwrap();
        let file = Config::from_sources(Some(&path), None, None).unwrap();
        assert_eq!(file.base_url.as_str(), "https://wiki.test/confluence/");
        assert_eq!(file.authorization, "Bearer file-secret");
        let env = Config::from_sources(
            Some(&path),
            Some("https://other.test".into()),
            Some("env-secret".into()),
        )
        .unwrap();
        assert_eq!(env.base_url.as_str(), "https://other.test/");
        assert_eq!(env.authorization, "Bearer env-secret");
        assert!(env.authorization.is_sensitive());
        let mixed = Config::from_sources(Some(&path), None, Some("env-secret".into())).unwrap();
        assert_eq!(mixed.base_url, file.base_url);
        assert_eq!(mixed.authorization, env.authorization);
    }

    #[test]
    fn missing_file_works_with_environment() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        assert!(
            Config::from_sources(
                Some(&missing),
                Some("https://wiki.test/".into()),
                Some("secret".into()),
            )
            .is_ok()
        );
        assert!(Config::from_sources(Some(&missing), None, None).is_err());
        assert!(Config::from_sources(None, Some("https://wiki.test".into()), None).is_err());
    }

    #[test]
    fn invalid_configuration_never_prints_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "token = 'secret-that-must-not-appear' invalid").unwrap();
        let error = Config::from_sources(
            Some(&path),
            Some("https://wiki.test".into()),
            Some("env-secret".into()),
        )
        .err()
        .unwrap();
        assert!(!format!("{error:#}").contains("secret"));
        assert!(Config::new("https://wiki.test", "").is_err());
        assert!(Config::new("https://wiki.test", "secret\nvalue").is_err());
        assert!(Config::new("https://wiki.test", " secret").is_err());
        assert!(Config::new("https://wiki.test", "secret\0value").is_err());
        for url in [
            "ftp://wiki.test",
            "https://user:password@wiki.test",
            "https://wiki.test?token=hidden",
            "https://wiki.test/#fragment",
            "",
        ] {
            assert!(Config::new(url, "secret").is_err(), "{url}");
        }
    }
}
