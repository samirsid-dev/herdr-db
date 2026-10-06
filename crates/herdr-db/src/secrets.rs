//! Passwords. Resolution order: `password_command` (Infisical, 1Password
//! CLI, Vault...), then the OS keyring, then a prompt in the pane. A password
//! only exists in memory, as a `SecretString`, while the connection opens:
//! never in a file, a log or an error message.

use anyhow::{Context, Result, bail};
use herdr_db_core::config::SourceConfig;
use secrecy::{ExposeSecret, SecretString};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub const KEYRING_SERVICE: &str = "herdr-db";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Keyring entries are indexed by data source and user.
fn account(source_id: &str, user: &str) -> String {
    format!("{source_id}:{user}")
}

/// Runs the command; its standard output is the secret and is never logged.
/// Standard error is logged, truncated.
pub async fn from_command(command: &[String]) -> Result<SecretString> {
    let (program, args) = command.split_first().context("password_command vide")?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("password_command : impossible de lancer `{program}`"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a?;
        b?;
        let status = child.wait().await?;
        anyhow::Ok((status, out, err))
    };
    let (status, out, err) =
        tokio::time::timeout(COMMAND_TIMEOUT, run).await.context("password_command : délai dépassé")??;
    let err_text: String = String::from_utf8_lossy(&err).chars().take(400).collect();
    if !err_text.trim().is_empty() {
        tracing::info!(program = %program, stderr = %err_text.trim(), "password_command stderr");
    }
    if !status.success() {
        bail!(
            "password_command `{program}` a échoué ({status}){}",
            if err_text.trim().is_empty() {
                String::new()
            } else {
                format!(" : {}", err_text.lines().next().unwrap_or("").trim())
            }
        );
    }
    let mut text = String::from_utf8(out).context("password_command : sortie non UTF-8")?;
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    if text.is_empty() {
        bail!("password_command `{program}` n'a rien renvoyé");
    }
    Ok(SecretString::from(text))
}

pub fn from_keyring(source_id: &str, user: &str) -> Result<Option<SecretString>> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &account(source_id, user)).context("trousseau du système indisponible")?;
    match entry.get_password() {
        Ok(password) => Ok(Some(SecretString::from(password))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("trousseau : {e}")),
    }
}

pub fn save_keyring(source_id: &str, user: &str, password: &SecretString) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &account(source_id, user)).context("trousseau du système indisponible")?;
    entry.set_password(password.expose_secret()).map_err(|e| anyhow::anyhow!("trousseau : {e}"))
}

pub fn delete_keyring(source_id: &str, user: &str) -> Result<bool> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &account(source_id, user)).context("trousseau du système indisponible")?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("trousseau : {e}")),
    }
}

/// Database user: configured, else the OS user (as libpq does).
pub fn user_of(source: &SourceConfig) -> String {
    source.user.clone().or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "postgres".to_string())
}

/// Non-interactive resolution: command, then keyring. `None`: try without a
/// password, then prompt if the server asks for one.
pub async fn resolve(source: &SourceConfig) -> Result<Option<SecretString>> {
    if let Some(command) = &source.password_command {
        return from_command(command).await.map(Some);
    }
    let source_id = source.id.clone();
    let user = user_of(source);
    match tokio::task::spawn_blocking(move || from_keyring(&source_id, &user)).await? {
        Ok(found) => Ok(found),
        Err(e) => {
            tracing::warn!(error = %e, "keyring unavailable");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_output_is_the_secret() {
        let secret = from_command(&["printf".into(), "s3cr3t\\n".into()]).await.unwrap();
        assert_eq!(secret.expose_secret(), "s3cr3t");
        assert!(format!("{secret:?}").contains("REDACTED"));
    }

    #[tokio::test]
    async fn failing_or_empty_commands_are_errors() {
        let error = from_command(&["sh".into(), "-c".into(), "echo nope >&2; exit 3".into()]).await.unwrap_err();
        assert!(error.to_string().contains("nope"));
        assert!(from_command(&["true".into()]).await.is_err());
        assert!(from_command(&["/nonexistent/x".into()]).await.is_err());
    }
}
