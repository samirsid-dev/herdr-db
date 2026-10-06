//! Logs go to a file in the state dir: standard output belongs to the
//! interface. A redaction layer masks `password=` patterns that a driver
//! error might carry, on top of never logging secrets in the first place.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;

pub fn init(logs_dir: &Path) {
    if herdr_db_store::create_private_dir(logs_dir).is_err() {
        return;
    }
    let path = logs_dir.join("herdr-db.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_SIZE) {
        let _ = std::fs::rename(&path, logs_dir.join("herdr-db.log.1"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = herdr_db_store::set_private(&path);
    let filter = EnvFilter::try_from_env("HERDR_DB_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(RedactingWriter(Arc::new(Mutex::new(file))))
        .try_init();
}

#[derive(Clone)]
struct RedactingWriter(Arc<Mutex<File>>);

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let redacted = redact(&text);
        let mut file = self.0.lock().unwrap_or_else(|e| e.into_inner());
        file.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

impl<'a> MakeWriter<'a> for RedactingWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Masks the value following `password=`, `pass=`, `pwd=` or `PGPASSWORD=`.
pub fn redact(text: &str) -> String {
    const KEYS: [&str; 4] = ["password=", "pass=", "pwd=", "pgpassword="];
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let matched = KEYS
            .iter()
            .find(|k| lower[i..].starts_with(*k) && (i == 0 || !lower.as_bytes()[i - 1].is_ascii_alphanumeric()));
        if let Some(key) = matched {
            out.push_str(&text[i..i + key.len()]);
            i += key.len();
            let rest = &text[i..];
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | ';' | ',' | '"' | '\'' | ')'))
                .unwrap_or(rest.len());
            out.push_str("[REDACTED]");
            i += end;
            continue;
        }
        let c = text[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_password_values() {
        assert_eq!(redact("host=x password=s3cr3t user=a"), "host=x password=[REDACTED] user=a");
        assert_eq!(
            redact("mysql://u:p@h/?pass=abc&x=1 PGPASSWORD=zz"),
            "mysql://u:p@h/?pass=[REDACTED]&x=1 PGPASSWORD=[REDACTED]"
        );
        assert_eq!(redact("bypass=1 compass=2"), "bypass=1 compass=2");
        assert_eq!(redact("é password=é"), "é password=[REDACTED]");
    }
}
