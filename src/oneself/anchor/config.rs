//! `/etc/pulse-null/anchor.toml`. The pulse file is never this config.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::AnchorError;

pub(crate) const DEFAULT_PATH: &str = "/etc/pulse-null/anchor.toml";

/// A token. `Debug` and `Display` do not show it.
pub struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret([redacted])")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

pub(crate) enum Spec {
    Local,
    Noop,
    GitHub {
        owner: String,
        repo: String,
        branch: String,
        dir: String,
        token_file: Option<PathBuf>,
        api_base: String,
    },
}

#[derive(Deserialize)]
struct FileConfig {
    #[serde(default)]
    backend: Vec<BackendToml>,
}

#[derive(Deserialize)]
struct BackendToml {
    kind: String,
    repo: Option<String>,
    #[serde(default = "default_branch")]
    branch: String,
    #[serde(default = "default_dir")]
    dir: String,
    token_file: Option<PathBuf>,
    api_base: Option<String>,
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_dir() -> String {
    "heads".to_string()
}

pub(crate) fn load_specs(config: Option<&Path>) -> Result<Vec<Spec>, AnchorError> {
    let path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH));
    check_config(&path, config.is_none())?;
    let text =
        std::fs::read_to_string(&path).map_err(|_| AnchorError::Config("unreadable".into()))?;
    parse_specs(&text)
}

pub(crate) fn parse_specs(text: &str) -> Result<Vec<Spec>, AnchorError> {
    let file: FileConfig =
        toml::from_str(text).map_err(|_| AnchorError::Config("not valid".into()))?;
    if file.backend.is_empty() {
        return Err(AnchorError::Config("no backend".into()));
    }
    file.backend.into_iter().map(spec_of).collect()
}

fn spec_of(item: BackendToml) -> Result<Spec, AnchorError> {
    match item.kind.as_str() {
        "local" => Ok(Spec::Local),
        "noop" => Ok(Spec::Noop),
        "github" => {
            let repo = item
                .repo
                .ok_or_else(|| AnchorError::Config("repo".into()))?;
            let (owner, name) = split_repo(&repo)?;
            let api_base = item
                .api_base
                .unwrap_or_else(|| "https://api.github.com".to_string());
            check_api_base(&api_base)?;
            if item.branch.is_empty() || item.dir.is_empty() {
                return Err(AnchorError::Config("branch".into()));
            }
            Ok(Spec::GitHub {
                owner,
                repo: name,
                branch: item.branch,
                dir: item.dir,
                token_file: item.token_file,
                api_base,
            })
        }
        _ => Err(AnchorError::Config("kind".into())),
    }
}

fn split_repo(repo: &str) -> Result<(String, String), AnchorError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(AnchorError::Config("repo".into()));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || owner.contains(' ')
        || name.contains(' ')
    {
        return Err(AnchorError::Config("repo".into()));
    }
    Ok((owner.to_string(), name.to_string()))
}

/// `https` is accepted. `http` is accepted only for `127.0.0.1` and `localhost`.
pub(crate) fn check_api_base(base: &str) -> Result<(), AnchorError> {
    let url = reqwest::Url::parse(base).map_err(|_| AnchorError::ApiBase)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AnchorError::ApiBase);
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback(url.host_str()) => Ok(()),
        _ => Err(AnchorError::ApiBase),
    }
}

fn loopback(host: Option<&str>) -> bool {
    host.is_some_and(|host| host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1")
}

fn check_config(path: &Path, require_root: bool) -> Result<(), AnchorError> {
    let meta = std::fs::metadata(path).map_err(|_| AnchorError::Config("unreadable".into()))?;
    if !meta.is_file() {
        return Err(AnchorError::Config("not a file".into()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(AnchorError::ConfigMode);
    }
    if require_root && meta.uid() != 0 {
        return Err(AnchorError::ConfigOwner);
    }
    Ok(())
}

/// `--token-file`, then the backend file, then `PULSE_NULL_ANCHOR_TOKEN_FILE`,
/// then `$CREDENTIALS_DIRECTORY/anchor-token`, then `PULSE_NULL_ANCHOR_TOKEN`.
pub(crate) fn resolve_token(
    cli: Option<&Path>,
    configured: Option<&Path>,
) -> Result<Secret, AnchorError> {
    let env_file = std::env::var("PULSE_NULL_ANCHOR_TOKEN_FILE").ok();
    let creds = std::env::var("CREDENTIALS_DIRECTORY").ok();
    let direct = std::env::var("PULSE_NULL_ANCHOR_TOKEN").ok();
    resolve_parts(
        cli,
        configured,
        env_file.as_deref(),
        creds.as_deref(),
        direct.as_deref(),
    )
}

pub(crate) fn resolve_parts(
    cli: Option<&Path>,
    configured: Option<&Path>,
    env_file: Option<&str>,
    credentials_dir: Option<&str>,
    direct: Option<&str>,
) -> Result<Secret, AnchorError> {
    if let Some(path) = cli {
        return read_token_file(path);
    }
    if let Some(path) = configured {
        return read_token_file(path);
    }
    if let Some(path) = env_file {
        return read_token_file(Path::new(path));
    }
    if let Some(dir) = credentials_dir {
        return read_token_file(&Path::new(dir).join("anchor-token"));
    }
    match direct.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => Ok(Secret::new(token.to_string())),
        None => Err(AnchorError::TokenMissing),
    }
}

fn read_token_file(path: &Path) -> Result<Secret, AnchorError> {
    let meta = std::fs::metadata(path).map_err(|_| AnchorError::TokenMissing)?;
    if !meta.is_file() {
        return Err(AnchorError::TokenMissing);
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AnchorError::TokenMode);
    }
    let text = std::fs::read_to_string(path).map_err(|_| AnchorError::TokenMissing)?;
    let token = text.trim();
    if token.is_empty() {
        return Err(AnchorError::TokenMissing);
    }
    Ok(Secret::new(token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_readable_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "pn108-token-do-not-leak\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_token_file(&path).unwrap_err();
        assert!(matches!(err, AnchorError::TokenMode), "{err}");
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains("pn108-token-do-not-leak"), "{rendered}");
        let secret = Secret::new("pn108-token-do-not-leak".into());
        let rendered = format!("{secret} {secret:?}");
        assert!(!rendered.contains("pn108-token-do-not-leak"), "{rendered}");
    }

    #[test]
    fn a_private_token_file_is_read_and_a_plain_http_base_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "kept-secret\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let secret = resolve_parts(Some(&path), None, None, None, None).unwrap();
        assert_eq!(secret.expose(), "kept-secret");
        assert!(check_api_base("http://example.com").is_err());
        assert!(check_api_base("http://127.0.0.1:9").is_ok());
        assert!(check_api_base("http://localhost:9").is_ok());
        assert!(check_api_base("https://api.github.com").is_ok());
        assert!(check_api_base("https://user:secret@api.github.com").is_err());
    }

    #[test]
    fn config_mode_and_owner_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.toml");
        std::fs::write(&path, "[[backend]]\nkind = \"noop\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(matches!(
            check_config(&path, false).unwrap_err(),
            AnchorError::ConfigMode
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_config(&path, false).is_ok());
        assert!(matches!(
            check_config(&path, true).unwrap_err(),
            AnchorError::ConfigOwner
        ));
        let specs =
            parse_specs("[[backend]]\nkind = \"local\"\n[[backend]]\nkind = \"noop\"\n").unwrap();
        assert!(matches!(specs[0], Spec::Local));
        assert!(matches!(specs[1], Spec::Noop));
        assert!(parse_specs(
            "[[backend]]\nkind = \"github\"\napi_base = \"http://example.com\"\nrepo = \"a/b\"\n"
        )
        .is_err());
    }
}
