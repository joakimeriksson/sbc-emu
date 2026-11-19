//! Shared script text and serial-log helpers. Adapters own commands, timing,
//! wait consumption and their additional escape sequences.
use std::collections::VecDeque;

fn parse(text: &str) -> VecDeque<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub fn load(path: Option<&str>) -> std::io::Result<VecDeque<String>> {
    match path {
        Some(path) => std::fs::read_to_string(path).map(|text| parse(&text)),
        None => Ok(VecDeque::new()),
    }
}

pub fn unescape(text: &str) -> String {
    text.replace("\\n", "\r")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
}

pub fn append_serial(log: &mut String, bytes: &[u8]) {
    log.push_str(&String::from_utf8_lossy(bytes));
    if log.len() > 1 << 20 {
        let mut cut = log.len() - (1 << 19);
        while !log.is_char_boundary(cut) {
            cut += 1;
        }
        log.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_preserve_inline_shell_comments_unicode_and_escape_text() {
        let script = parse("  # comment\r\n\n serial echo '#keep'\\r\n wait-serial blåbär\n");
        assert_eq!(script, ["serial echo '#keep'\\r", "wait-serial blåbär"]);
        // The adapters keep their existing extensions: Pi uses \e, whereas
        // SerialBoard uses \x03. Sharing the base must not add either implicitly.
        assert_eq!(unescape("å\\n\\r\\t\\e\\x03"), "å\r\r\t\\e\\x03");
    }

    #[test]
    fn serial_log_truncation_never_splits_a_utf8_character() {
        let mut log = "a".repeat((1 << 19) - 1);
        log.push('€');
        log.push_str(&"z".repeat((1 << 19) - 3));
        let mut old = log.clone();
        old.push_str("yz");
        assert!(!old.is_char_boundary(old.len() - (1 << 19)));
        append_serial(&mut log, b"yz");
        assert_eq!(log.len(), (1 << 19) - 1);
        assert_eq!(log, "z".repeat((1 << 19) - 3) + "yz");
    }

    #[test]
    fn invalid_serial_bytes_remain_matchable_without_exceeding_the_limit() {
        let mut log = "界".repeat((1 << 20) / 3);
        append_serial(&mut log, b"\xff\xfeREADY");
        assert!(log.len() <= 1 << 19);
        assert!(log.ends_with("��READY"));
    }
}
