//! Small shell-facing helpers shared by the CLI and the PTY wrapper.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// The outer terminal must relay bytes, including Ctrl+C, to the child PTY.
/// Restore its original settings on every normal/error return.
pub(crate) struct RawTerminal(Option<libc::termios>);

impl RawTerminal {
    pub(crate) fn enter(preserve_output: bool) -> std::io::Result<Self> {
        if !std::io::stdin().is_terminal() {
            return Ok(Self(None));
        }
        unsafe {
            let mut saved = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if preserve_output {
                raw.c_oflag = saved.c_oflag;
            }
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(Some(saved)))
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved);
            }
        }
    }
}

/// `$SHELL` if set and plausible, else `/bin/sh`.
pub fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Shell name in lower case, e.g. `zsh` or `bash`.
pub fn shell_name(shell: &Path) -> String {
    shell
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "sh".into())
}

/// Strip ANSI escape sequences so captured output can be matched and displayed.
/// Handles CSI sequences (`ESC [ … final`), OSC strings terminated by BEL or ST,
/// and intermediate-byte escapes such as `ESC ( B`. Parameter bytes are skipped
/// until a final byte in `0x40..=0x7e`, which is what actually ends a CSI
/// sequence.
pub fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            // Copy a run of plain bytes at once rather than char by char.
            let start = i;
            while i < bytes.len() && bytes[i] != 0x1b {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        // ESC confirmed at bytes[i].
        i += 1;
        match bytes.get(i).copied() {
            Some(b'[') => {
                i += 1;
                while i < bytes.len()
                    && !bytes[i].is_ascii_control()
                    && !(0x40..=0x7e).contains(&bytes[i])
                {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                // OSC: runs until BEL, or ESC \ (ST).
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(c) if (0x20..=0x2f).contains(&c) => {
                // Charset designation such as `ESC ( B`: intermediate byte plus
                // the one byte that completes the sequence.
                i += 2;
            }
            Some(_) => {
                // Two-character escape.
                i += 1;
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strips_cursor_movement() {
        assert_eq!(strip_ansi("a\x1b[2Kb"), "ab");
    }

    #[test]
    fn strips_osc_title_sequences() {
        assert_eq!(strip_ansi("\x1b]0;title\x07body"), "body");
    }

    #[test]
    fn strips_osc_terminated_by_st() {
        assert_eq!(
            strip_ansi("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn strips_charset_designation() {
        assert_eq!(strip_ansi("a\x1b(Bb"), "ab");
    }

    #[test]
    fn leaves_plain_text_intact() {
        assert_eq!(strip_ansi("plain text\n"), "plain text\n");
    }

    #[test]
    fn strips_multibyte_safe() {
        // `é` is two bytes and must survive untouched once the CSI is removed.
        assert_eq!(strip_ansi("\x1b[32mé\x1b[0m ✅"), "é ✅");
    }

    #[test]
    fn trailing_escape_is_dropped() {
        assert_eq!(strip_ansi("done\x1b"), "done");
    }
}
