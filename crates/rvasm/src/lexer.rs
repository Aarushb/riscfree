//! Line tokenizer. Produces positioned tokens; the parser in `asm` does the
//! grammar. No regex anywhere, per the performance budget.

use crate::SourcePos;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Identifier: mnemonics, directives, register names, labels. RARS-style
    /// labels may contain `.` and `$`; a leading `%` marks macro parameters
    /// (and RARS `%hi`-style operator names).
    Ident(String),
    /// Integer literal (decimal, 0x hex, 0b binary) or character literal.
    Int(i64),
    /// Floating-point literal: `1.5`, `-3.25e2`, `inf`, `nan`. Only the
    /// `.float`/`.double` directives accept these, matching RARS.
    Float(f64),
    /// String literal with escapes already applied.
    Str(String),
    Comma,
    Colon,
    LParen,
    RParen,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub pos: SourcePos,
}

pub fn lex_line(src: &str, pos: SourcePos, diags: &mut Vec<crate::Diagnostic>) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        let col = i as u32;
        let at = SourcePos { col, ..pos };
        match c {
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => break,
            b',' => {
                out.push(Token { tok: Tok::Comma, pos: at });
                i += 1;
            }
            b':' => {
                out.push(Token { tok: Tok::Colon, pos: at });
                i += 1;
            }
            b'(' => {
                out.push(Token { tok: Tok::LParen, pos: at });
                i += 1;
            }
            b')' => {
                out.push(Token { tok: Tok::RParen, pos: at });
                i += 1;
            }
            b'"' => {
                let (s, next) = lex_string(bytes, i, at, diags);
                out.push(Token { tok: Tok::Str(s), pos: at });
                i = next;
            }
            b'\'' => {
                let (v, next) = lex_char(bytes, i, at, diags);
                out.push(Token { tok: Tok::Int(v), pos: at });
                i = next;
            }
            b'-' | b'+' if i + 1 < bytes.len() && (bytes[i + 1].is_ascii_digit() || is_float_start(&bytes[i + 1..])) => {
                let (tok, next) = lex_number_or_float(bytes, i, at, diags);
                out.push(Token { tok, pos: at });
                i = next;
            }
            c if c.is_ascii_digit() => {
                let (tok, next) = lex_number_or_float(bytes, i, at, diags);
                out.push(Token { tok, pos: at });
                i = next;
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'.' || c == b'$' || c == b'%' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'$' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let text = &src[start..i];
                out.push(Token { tok: Tok::Ident(text.to_string()), pos: at });
            }
            _ => {
                diags.push(crate::Diagnostic::error(
                    "E-LEX",
                    format!("unexpected character '{}'", c as char),
                    at,
                ));
                i += 1;
            }
        }
    }
    out
}

fn lex_string(bytes: &[u8], start: usize, pos: SourcePos, diags: &mut Vec<crate::Diagnostic>) -> (String, usize) {
    let mut s = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return (s, i + 1),
            b'\\' => {
                if i + 1 < bytes.len() {
                    let e = bytes[i + 1];
                    match e {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'0' => s.push('\0'),
                        b'\\' => s.push('\\'),
                        b'"' => s.push('"'),
                        b'\'' => s.push('\''),
                        other => {
                            diags.push(crate::Diagnostic::error(
                                "E-ESCAPE",
                                format!("unknown escape '\\{}'", other as char),
                                pos,
                            ));
                            s.push(other as char);
                        }
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b => {
                // Collect a full UTF-8 scalar; source is a Rust str so slicing
                // at char boundaries via from_utf8 on the raw bytes is safe to
                // do one char at a time.
                let ch_len = utf8_len(b);
                if let Ok(chunk) = std::str::from_utf8(&bytes[i..(i + ch_len).min(bytes.len())]) {
                    s.push_str(chunk);
                    i += ch_len;
                } else {
                    i += 1;
                }
            }
        }
    }
    diags.push(crate::Diagnostic::error("E-STR", "unterminated string literal", pos));
    (s, bytes.len())
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn lex_char(bytes: &[u8], start: usize, pos: SourcePos, diags: &mut Vec<crate::Diagnostic>) -> (i64, usize) {
    // bytes[start] is the opening quote.
    let err = |diags: &mut Vec<crate::Diagnostic>| {
        diags.push(crate::Diagnostic::error("E-CHAR", "unterminated character literal", pos));
        (0, start + 1)
    };
    if start + 2 >= bytes.len() {
        return err(diags);
    }
    if bytes[start + 1] == b'\\' {
        if start + 3 >= bytes.len() || bytes[start + 3] != b'\'' {
            return err(diags);
        }
        let e = bytes[start + 2];
        let v = match e {
            b'n' => b'\n' as i64,
            b't' => b'\t' as i64,
            b'0' => 0,
            b'\\' => b'\\' as i64,
            b'\'' => b'\'' as i64,
            b'"' => b'"' as i64,
            other => {
                diags.push(crate::Diagnostic::error(
                    "E-ESCAPE",
                    format!("unknown escape '\\{}'", other as char),
                    pos,
                ));
                other as i64
            }
        };
        (v, start + 4)
    } else if bytes[start + 2] == b'\'' {
        (bytes[start + 1] as i64, start + 3)
    } else {
        err(diags)
    }
}

/// True when the bytes ahead begin an `inf`/`nan` float literal (a sign is
/// allowed because the sign arm checks before dispatching).
fn is_float_start(rest: &[u8]) -> bool {
    rest.starts_with(b"inf") || rest.starts_with(b"nan")
}

/// Scan an integer or floating literal. Returns a float token when the text
/// carries a fraction, an exponent, or is inf/nan; everything else keeps the
/// integer path (hex/binary included) so existing diagnostics are unchanged.
fn lex_number_or_float(bytes: &[u8], start: usize, pos: SourcePos, diags: &mut Vec<crate::Diagnostic>) -> (Tok, usize) {
    let mut i = start;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    if is_float_start(&bytes[i..]) {
        if bytes[i] == b'i' {
            let v = if bytes[start] == b'-' { f64::NEG_INFINITY } else { f64::INFINITY };
            return (Tok::Float(v), i + 3);
        }
        return (Tok::Float(f64::NAN), i + 3);
    }
    // Hex/binary literals are always integers; plain decimal digits may
    // continue into a fraction/exponent.
    if bytes[i] == b'0' && i + 1 < bytes.len() && ((bytes[i + 1] | 0x20) == b'x' || (bytes[i + 1] | 0x20) == b'b') {
        let (v, next) = lex_number(bytes, start, pos, diags);
        return (Tok::Int(v), next);
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] | 0x20) == b'e' {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i == digits_start {
        let text = std::str::from_utf8(&bytes[start..i]).unwrap_or("0");
        diags.push(crate::Diagnostic::error("E-NUM", format!("invalid number '{text}'"), pos));
        return (Tok::Int(0), i);
    }
    let text = std::str::from_utf8(&bytes[start..i]).unwrap_or("0");
    if !is_float {
        let (v, next) = lex_number(bytes, start, pos, diags);
        return (Tok::Int(v), next);
    }
    match text.parse::<f64>() {
        Ok(v) => (Tok::Float(v), i),
        Err(_) => {
            diags.push(crate::Diagnostic::error("E-NUM", format!("invalid number '{text}'"), pos));
            (Tok::Float(0.0), i)
        }
    }
}

fn lex_number(bytes: &[u8], start: usize, pos: SourcePos, diags: &mut Vec<crate::Diagnostic>) -> (i64, usize) {
    let mut i = start;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    let digits_start = i;
    if bytes[i] == b'0' && i + 1 < bytes.len() && (bytes[i + 1] | 0x20) == b'x' {
        i += 2;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
    } else if bytes[i] == b'0' && i + 1 < bytes.len() && (bytes[i + 1] | 0x20) == b'b' {
        i += 2;
        while i < bytes.len() && (bytes[i] == b'0' || bytes[i] == b'1') {
            i += 1;
        }
    } else {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let text = std::str::from_utf8(&bytes[start..i]).unwrap_or("0");
    if i == digits_start {
        diags.push(crate::Diagnostic::error("E-NUM", format!("invalid number '{text}'"), pos));
        return (0, i);
    }
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text.trim_start_matches('+')),
    };
    let parsed = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
    } else if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        i64::from_str_radix(bin, 2)
    } else {
        body.parse::<i64>()
    };
    let v = match parsed {
        Ok(n) => sign * n,
        Err(_) => {
            diags.push(crate::Diagnostic::error("E-NUM", format!("invalid number '{text}'"), pos));
            0
        }
    };
    (v, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    fn pos() -> SourcePos {
        SourcePos { file: 0, line: 1, col: 0 }
    }

    #[test]
    fn tokens_basic_line() {
        let mut d = Vec::new();
        let toks = lex_line("loop: addi a0, a0, -1 # decrement", pos(), &mut d);
        assert!(d.is_empty());
        assert_eq!(toks.len(), 8);
        assert_eq!(toks[0], Token { tok: Tok::Ident("loop".into()), pos: toks[0].pos });
        assert_eq!(toks[1].tok, Tok::Colon);
        assert_eq!(toks[2].tok, Tok::Ident("addi".into()));
        assert_eq!(toks[7].tok, Tok::Int(-1));
    }

    #[test]
    fn string_and_escapes() {
        let mut d = Vec::new();
        let toks = lex_line(".asciz \"a\\nb\\\"c\"", pos(), &mut d);
        assert_eq!(toks[1].tok, Tok::Str("a\nb\"c".into()));
    }

    #[test]
    fn number_bases() {
        let mut d = Vec::new();
        let toks = lex_line("0x10 0b101 -3", pos(), &mut d);
        assert_eq!(toks[0].tok, Tok::Int(16));
        assert_eq!(toks[1].tok, Tok::Int(5));
        assert_eq!(toks[2].tok, Tok::Int(-3));
        assert!(d.is_empty());
    }

    #[test]
    fn float_literals() {
        let mut d = Vec::new();
        let toks = lex_line("1.25 -0.5 1.5e-3 2e10 -inf -nan 42", pos(), &mut d);
        assert!(d.is_empty());
        assert_eq!(toks[0].tok, Tok::Float(1.25));
        assert_eq!(toks[1].tok, Tok::Float(-0.5));
        assert_eq!(toks[2].tok, Tok::Float(1.5e-3));
        assert_eq!(toks[3].tok, Tok::Float(2e10));
        assert_eq!(toks[4].tok, Tok::Float(f64::NEG_INFINITY));
        assert!(matches!(toks[5].tok, Tok::Float(v) if v.is_nan()));
        assert_eq!(toks[6].tok, Tok::Int(42));
        // Bare `inf`/`nan` stay identifiers (the .float/.double directive
        // accepts them as such); only signed forms route through the
        // number lexer.
        let mut d = Vec::new();
        let toks = lex_line("inf nan", pos(), &mut d);
        assert_eq!(toks[0].tok, Tok::Ident("inf".into()));
        assert_eq!(toks[1].tok, Tok::Ident("nan".into()));
    }

    #[test]
    fn percent_starts_parameter_names() {
        // Macro parameters lex as single identifiers so the expander can
        // match them textually.
        let mut d = Vec::new();
        let toks = lex_line(".macro mvadd(%dst, %src)", pos(), &mut d);
        assert!(d.is_empty());
        assert_eq!(toks[2].tok, Tok::LParen);
        assert_eq!(toks[3].tok, Tok::Ident("%dst".into()));
        assert_eq!(toks[5].tok, Tok::Ident("%src".into()));
    }

    #[test]
    fn unterminated_string_is_error() {
        let mut d = Vec::new();
        let toks = lex_line("\"oops", pos(), &mut d);
        assert_eq!(toks[0].tok, Tok::Str("oops".into()));
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!(d[0].code, "E-STR");
    }
}
