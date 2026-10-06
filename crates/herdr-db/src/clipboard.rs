//! Copy to the clipboard: the platform tool when present (pbcopy, wl-copy,
//! xclip, xsel), plus an OSC 52 sequence for terminals that forward it
//! (remote sessions).

use base64_lite::encode;
use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> bool {
    let native = native_copy(text);
    osc52(text);
    native
}

fn native_copy(text: &str) -> bool {
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]]
    };
    for candidate in candidates {
        let Ok(mut child) = Command::new(candidate[0])
            .args(&candidate[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return true;
        }
    }
    false
}

fn osc52(text: &str) {
    // Terminals cap OSC 52 payloads; skip what would be dropped anyway.
    if text.len() > 100_000 {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{}\x07", encode(text.as_bytes()));
    let _ = stdout.flush();
}

mod base64_lite {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn rfc4648_vectors() {
            for (input, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")]
            {
                assert_eq!(super::encode(input.as_bytes()), expected);
            }
        }
    }
}
