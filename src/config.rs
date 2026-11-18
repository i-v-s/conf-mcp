use anyhow::{Context, Result, bail};
use reqwest::header::HeaderValue;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use url::Url;

pub struct Config {
    pub base_url: Url,
    pub authorization: HeaderValue,
    pub download_dir: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    base_url: Option<String>,
    token: Option<String>,
    download_dir: Option<String>,
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
        let mut config = Self::new(&url, &token)?;
        let home = env::var_os("HOME").map(PathBuf::from);
        let xdg_cache = env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
        config.download_dir = download_directory(
            file.download_dir.as_deref(),
            home.as_deref(),
            xdg_cache.as_deref(),
        )?;
        Ok(config)
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
            download_dir: None,
        })
    }
}

fn download_directory(
    configured: Option<&str>,
    home: Option<&Path>,
    xdg_cache: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if let Some(value) = configured {
        let path = if let Some(rest) = value.strip_prefix("~/") {
            home.context("Для download_dir с ~/ требуется HOME")?
                .join(rest.trim_start_matches('/'))
        } else {
            PathBuf::from(value)
        };
        if !path.is_absolute() {
            bail!("download_dir должен быть непустым абсолютным путём или начинаться с ~/");
        }
        return Ok(Some(path));
    }
    if let Some(root) = xdg_cache.filter(|path| path.is_absolute()) {
        return Ok(Some(root.join("conf-mcp/images")));
    }
    Ok(home
        .filter(|path| path.is_absolute())
        .map(|root| root.join(".cache/conf-mcp/images")))
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
    fn download_directory_accepts_absolute_and_home_paths_and_keeps_defaults() {
        let home = Path::new("/home/test");
        assert_eq!(
            download_directory(Some("/tmp/conf images"), Some(home), None).unwrap(),
            Some(PathBuf::from("/tmp/conf images"))
        );
        assert_eq!(
            download_directory(Some("~/Downloads/confluence"), Some(home), None).unwrap(),
            Some(home.join("Downloads/confluence"))
        );
        assert_eq!(
            download_directory(None, Some(home), Some(Path::new("/var/cache/test"))).unwrap(),
            Some(PathBuf::from("/var/cache/test/conf-mcp/images"))
        );
        assert_eq!(
            download_directory(None, Some(home), Some(Path::new("relative"))).unwrap(),
            Some(home.join(".cache/conf-mcp/images"))
        );
        assert_eq!(download_directory(None, None, None).unwrap(), None);
        for value in ["", "relative/images", "~someone/images", "~"] {
            assert!(download_directory(Some(value), Some(home), None).is_err());
        }
        assert!(download_directory(Some("~/images"), None, None).is_err());
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.toml");
        let directory = temporary.path().join("not-created/images");
        fs::write(
            &path,
            format!(
                "base_url = 'https://wiki.test'\ntoken = 'secret'\ndownload_dir = '{}'\n",
                directory.display()
            ),
        )
        .unwrap();
        let config = Config::from_sources(Some(&path), None, None).unwrap();
        assert_eq!(config.download_dir, Some(directory.clone()));
        assert!(!directory.exists());
        fs::write(
            &path,
            "base_url = 'https://wiki.test'\ntoken = 'secret'\ndownload_dir = ''\n",
        )
        .unwrap();
        let error = Config::from_sources(Some(&path), None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("download_dir"));
        assert!(!error.contains("secret"));
    }

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
