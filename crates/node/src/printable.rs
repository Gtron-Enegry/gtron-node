//! Text from the platform or a TRON node, made safe to print in a terminal.

/// `s` without control characters (terminal escapes could rewrite what is on screen, such as
/// the message setup is about to sign), cut to 500 characters.
pub fn printable(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(500)
        .collect();
    if s.chars().count() > 500 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        assert_eq!(printable("ok\x1b[2K\x1b[1Ahidden\r\n"), "ok[2K[1Ahidden\n");
        assert_eq!(printable(&"a".repeat(600)).chars().count(), 501);
    }
}
