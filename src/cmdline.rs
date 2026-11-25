//! Hilfsfunktionen für Windows-Kommandozeilen (Dienst-Befehlszeile).

/// Windows-Kommandozeilen-Quoting (MSVCRT-Regeln).
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("run"), "run");
        assert_eq!(
            quote_arg(r"C:\Program Files\ptp\ptpmaster.exe"),
            r#""C:\Program Files\ptp\ptpmaster.exe""#
        );
        assert_eq!(
            quote_arg("--interface=Ethernet 2"),
            r#""--interface=Ethernet 2""#
        );
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote_arg(r"C:\nospace\x"), r"C:\nospace\x");
        assert_eq!(quote_arg(""), r#""""#);
    }
}
