//! `/etc/pulse-null/anchor.toml`. The pulse file is never this config.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::AnchorError;

pub(crate) const DEFAULT_PATH: &str = "/etc/pulse-null/anchor.toml";

const CONFIG_CAP: u64 = 64 * 1024;
const TOKEN_CAP: u64 = 4 * 1024;

/// A token. `Debug` and `Display` do not show it. Drop overwrites the bytes.
pub struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        scrub(&mut self.0);
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

/// Overwrite a secret's bytes before the allocation is freed.
fn scrub(text: &mut String) {
    // SAFETY: `as_bytes_mut` is this `String`'s own buffer. Each volatile write
    // covers one byte of that buffer so the secret does not stay readable
    // after this function returns.
    unsafe {
        for byte in text.as_bytes_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    std::hint::black_box(text.as_bytes());
}

#[derive(Debug)]
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
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    backend: Vec<BackendToml>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    let file = open_config(&path, config.is_none())?;
    let text = read_capped(&file, CONFIG_CAP)?;
    parse_specs(&text)
}

pub(crate) fn parse_specs(text: &str) -> Result<Vec<Spec>, AnchorError> {
    let file: FileConfig =
        toml::from_str(text).map_err(|err| AnchorError::Config(err.to_string()))?;
    if file.backend.is_empty() {
        return Err(AnchorError::Config("no backend".into()));
    }
    let mut specs = Vec::with_capacity(file.backend.len());
    let mut local = false;
    let mut noop = false;
    let mut github = Vec::new();
    for item in file.backend {
        let spec = spec_of(item)?;
        match &spec {
            Spec::Local if local => {
                return Err(AnchorError::Config("duplicate local".into()));
            }
            Spec::Local => local = true,
            Spec::Noop if noop => {
                return Err(AnchorError::Config("duplicate noop".into()));
            }
            Spec::Noop => noop = true,
            Spec::GitHub {
                owner,
                repo,
                branch,
                dir,
                ..
            } => {
                let key = (owner.clone(), repo.clone(), branch.clone(), dir.clone());
                if github.contains(&key) {
                    return Err(AnchorError::Config("duplicate github".into()));
                }
                github.push(key);
            }
        }
        specs.push(spec);
    }
    Ok(specs)
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
            check_branch(&item.branch)?;
            check_dir(&item.dir)?;
            let api_base = item
                .api_base
                .unwrap_or_else(|| "https://api.github.com".to_string());
            check_api_base(&api_base)?;
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
    if name.contains('/') || !part_ok(owner) || !part_ok(name) {
        return Err(AnchorError::Config("repo".into()));
    }
    Ok((owner.to_string(), name.to_string()))
}

/// `owner` and `repo` are one path segment each: letters, digits, `.`, `_`, `-`.
pub(crate) fn check_repo_parts(owner: &str, repo: &str) -> Result<(), AnchorError> {
    if part_ok(owner) && part_ok(repo) {
        Ok(())
    } else {
        Err(AnchorError::Config("repo".into()))
    }
}

fn part_ok(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// No `..`, no leading `-` or `/`, and no control characters.
pub(crate) fn check_branch(branch: &str) -> Result<(), AnchorError> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.contains("..")
        || branch.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        Err(AnchorError::Config("branch".into()))
    } else {
        Ok(())
    }
}

/// Relative, with no empty, `.`, or `..` component.
pub(crate) fn check_dir(dir: &str) -> Result<(), AnchorError> {
    if dir.is_empty() || dir.starts_with('/') || dir.starts_with('\\') {
        return Err(AnchorError::Config("dir".into()));
    }
    let mut any = false;
    for part in dir.split('/') {
        any = true;
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part
                .bytes()
                .any(|byte| byte < 0x20 || byte == 0x7f || byte.is_ascii_whitespace())
        {
            return Err(AnchorError::Config("dir".into()));
        }
    }
    if !any {
        return Err(AnchorError::Config("dir".into()));
    }
    Ok(())
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

fn open_nofollow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn open_config(path: &Path, require_root: bool) -> Result<std::fs::File, AnchorError> {
    let file = open_nofollow(path).map_err(|err| {
        if err.raw_os_error() == Some(libc::ELOOP) {
            AnchorError::Config("is a symlink".into())
        } else {
            AnchorError::Config("unreadable".into())
        }
    })?;
    inspect_config(&file, require_root)?;
    Ok(file)
}

fn inspect_config(file: &std::fs::File, require_root: bool) -> Result<(), AnchorError> {
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Config("unreadable".into()))?;
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

#[cfg(test)]
fn check_config(path: &Path, require_root: bool) -> Result<(), AnchorError> {
    open_config(path, require_root).map(|_| ())
}

fn read_capped(file: &std::fs::File, cap: u64) -> Result<String, AnchorError> {
    let meta = file
        .metadata()
        .map_err(|_| AnchorError::Config("unreadable".into()))?;
    if meta.len() > cap {
        return Err(AnchorError::Config("too large".into()));
    }
    let mut text = String::new();
    file.take(cap)
        .read_to_string(&mut text)
        .map_err(|_| AnchorError::Config("unreadable".into()))?;
    if text.len() as u64 > cap {
        return Err(AnchorError::Config("too large".into()));
    }
    Ok(text)
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
    let file = open_nofollow(path).map_err(|err| {
        if err.raw_os_error() == Some(libc::ELOOP) {
            AnchorError::TokenSymlink
        } else {
            AnchorError::TokenMissing
        }
    })?;
    let meta = file.metadata().map_err(|_| AnchorError::TokenMissing)?;
    if !meta.is_file() {
        return Err(AnchorError::TokenMissing);
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AnchorError::TokenMode);
    }
    if meta.len() > TOKEN_CAP {
        return Err(AnchorError::TokenMissing);
    }
    let mut text = String::new();
    file.take(TOKEN_CAP)
        .read_to_string(&mut text)
        .map_err(|_| AnchorError::TokenMissing)?;
    if text.len() as u64 > TOKEN_CAP {
        return Err(AnchorError::TokenMissing);
    }
    let token = text.trim().to_string();
    scrub(&mut text);
    if token.is_empty() {
        return Err(AnchorError::TokenMissing);
    }
    Ok(Secret::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path, bits: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
    }

    #[test]
    fn a_group_readable_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "pn108-token-do-not-leak\n").unwrap();
        mode(&path, 0o644);
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
        mode(&path, 0o600);
        let secret = resolve_parts(Some(&path), None, None, None, None).unwrap();
        assert_eq!(secret.expose(), "kept-secret");
        assert!(check_api_base("http://example.com").is_err());
        assert!(check_api_base("http://127.0.0.1:9").is_ok());
        assert!(check_api_base("http://localhost:9").is_ok());
        assert!(check_api_base("https://api.github.com").is_ok());
        assert!(check_api_base("https://user:secret@api.github.com").is_err());
    }

    #[test]
    fn a_symlink_config_or_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "pn108-token-do-not-leak\n").unwrap();
        mode(&token, 0o600);
        let link = dir.path().join("token-link");
        std::os::unix::fs::symlink(&token, &link).unwrap();
        let err = read_token_file(&link).unwrap_err();
        assert!(matches!(err, AnchorError::TokenSymlink), "{err}");
        assert!(err.to_string().contains("symlink"), "{err}");

        let cfg = dir.path().join("anchor.toml");
        std::fs::write(&cfg, "[[backend]]\nkind = \"noop\"\n").unwrap();
        mode(&cfg, 0o600);
        let cfg_link = dir.path().join("anchor-link.toml");
        std::os::unix::fs::symlink(&cfg, &cfg_link).unwrap();
        let err = check_config(&cfg_link, false).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(load_specs(Some(&cfg_link)).is_err());
    }

    #[test]
    fn config_mode_and_owner_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.toml");
        std::fs::write(&path, "[[backend]]\nkind = \"noop\"\n").unwrap();
        mode(&path, 0o664);
        assert!(matches!(
            check_config(&path, false).unwrap_err(),
            AnchorError::ConfigMode
        ));
        mode(&path, 0o600);
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

    #[test]
    fn a_misspelt_key_is_named_and_duplicates_are_refused() {
        let api =
            parse_specs("[[backend]]\nkind = \"local\"\napi-base = \"https://example.com\"\n")
                .unwrap_err();
        assert!(api.to_string().contains("api-base"), "{api}");
        let token = parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"owner/repo\"\ntokenfile = \"/tmp/t\"\n",
        )
        .unwrap_err();
        assert!(token.to_string().contains("tokenfile"), "{token}");
        let extra = parse_specs("extra = 1\n[[backend]]\nkind = \"noop\"\n").unwrap_err();
        assert!(extra.to_string().contains("extra"), "{extra}");

        let local = parse_specs("[[backend]]\nkind = \"local\"\n[[backend]]\nkind = \"local\"\n")
            .unwrap_err();
        assert!(local.to_string().contains("duplicate local"), "{local}");
        let noop = parse_specs("[[backend]]\nkind = \"noop\"\n[[backend]]\nkind = \"noop\"\n")
            .unwrap_err();
        assert!(noop.to_string().contains("duplicate noop"), "{noop}");
        let same = parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\n[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\n",
        )
        .unwrap_err();
        assert!(same.to_string().contains("duplicate github"), "{same}");
        let other = parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\n[[backend]]\nkind = \"github\"\nrepo = \"o/other\"\n",
        )
        .unwrap();
        assert_eq!(other.len(), 2);
    }

    #[test]
    fn repo_branch_and_dir_are_checked() {
        for repo in [
            "../..", ".", "..", "a/b/c", "owner/..", "./repo", "a/", "/b", "",
        ] {
            let err = parse_specs(&format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"{repo}\"\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("repo"), "{repo}: {err}");
        }
        assert!(
            parse_specs("[[backend]]\nkind = \"github\"\nrepo = \"owner.name/repo.ok\"\n").is_ok()
        );
        for branch in ["-main", "/main", "a/../b", ""] {
            let err = parse_specs(&format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\nbranch = \"{branch}\"\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("branch"), "{branch:?}: {err}");
        }
        let control = parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\nbranch = \"ma\\u001bin\"\n",
        )
        .unwrap_err();
        assert!(control.to_string().contains("branch"), "{control}");
        assert!(parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\nbranch = \"feature/foo\"\n"
        )
        .is_ok());
        for dir in ["/abs", "heads/../x", "heads//x", ".", "..", "heads/./x"] {
            let err = parse_specs(&format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\ndir = \"{dir}\"\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("dir"), "{dir}: {err}");
        }
        assert!(parse_specs(
            "[[backend]]\nkind = \"github\"\nrepo = \"o/r\"\ndir = \"heads/sub\"\n"
        )
        .is_ok());
    }
}
