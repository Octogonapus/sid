/// Escape pasted text so it can sit safely inside a sed `s///` (or similar) expression.
///
/// Escapes `\`, `/` (default delimiter), and `&` (replacement metacharacter), and turns
/// newlines into `\n` so a multi-line paste stays on the single-line expression bar.
pub fn escape_sed_paste(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '&' => out.push_str("\\&"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_delimiter_backslash_and_ampersand() {
        assert_eq!(escape_sed_paste(r"a/b\c&d"), r"a\/b\\c\&d");
    }

    #[test]
    fn flattens_newlines() {
        assert_eq!(escape_sed_paste("foo\nbar\r\nbaz"), r"foo\nbar\nbaz");
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(escape_sed_paste("hello world"), "hello world");
    }
}
