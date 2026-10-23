//! Zero-regex, low-footprint ANSI escape sequence stripper (`Utilities\AnsiText` equivalent).

use std::borrow::Cow;

/// Strip ANSI CSI, OSC, and 2-byte escape sequences from child stdout/stderr lines.
/// Returns `Cow::Borrowed` without allocating when the input contains no escape codes.
pub fn strip_ansi_codes(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0x1b) {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    // CSI sequence: ESC [ ... final byte in 0x40..=0x7E
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence: ESC ] ... terminated by BEL (\x07) or ST (ESC \)
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(c) if ('\x40'..='\x5f').contains(&c) => {
                    // 2-byte Fe escape sequence
                    chars.next();
                }
                _ => {}
            }
        } else {
            out.push(ch);
        }
    }

    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_codes_should_return_borrowed_when_no_escape_present() {
        // Arrange
        let line = "Ready in 142ms on http://127.0.0.1:3000";

        // Act
        let stripped = strip_ansi_codes(line);

        // Assert
        assert!(matches!(stripped, Cow::Borrowed(_)));
        assert_eq!(stripped, line);
    }

    #[test]
    fn strip_ansi_codes_should_remove_color_and_formatting_sequences() {
        // Arrange
        let colored = "\x1b[32m\x1b[1m▲ Next.js 15.0.0\x1b[22m\x1b[39m - \x1b[36mLocal:\x1b[0m http://localhost:3000";

        // Act
        let stripped = strip_ansi_codes(colored);

        // Assert
        assert_eq!(stripped, "▲ Next.js 15.0.0 - Local: http://localhost:3000");
    }

    #[test]
    fn strip_ansi_codes_should_remove_osc_hyperlinks() {
        // Arrange
        let osc = "Visit \x1b]8;;http://127.0.0.1:3000\x07localhost\x1b]8;;\x07 now";

        // Act
        let stripped = strip_ansi_codes(osc);

        // Assert
        assert_eq!(stripped, "Visit localhost now");
    }
}
