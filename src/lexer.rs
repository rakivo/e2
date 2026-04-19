use crate::color::Color;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TokenKind {
    Default,
    Comment,
    String,
    Number,
    Keyword,
    Type,
    Punct,
    Macro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Token {
    pub start: u32,
    /// High byte = TokenKind (as u8), low 3 bytes = length.
    /// Max token length: 16,777,215 bytes. Fine.
    kind_len: u32,
}

impl Token {
    #[inline(always)]
    pub fn new(kind: TokenKind, start: usize, len: usize) -> Self {
        Self {
            start: start as u32,
            kind_len: ((kind as u32) << 24) | (len as u32 & 0x00FF_FFFF),
        }
    }

    #[inline(always)]
    pub fn kind(self) -> TokenKind {
        unsafe { std::mem::transmute((self.kind_len >> 24) as u8) }
    }

    #[inline(always)]
    pub fn len(self) -> u32 {
        self.kind_len & 0x00FF_FFFF
    }
}

// base16-charcoal-dark
// base00 #0f0b05  bg
// base01 #231b0e  bg highlight
// base02 #2a2012  selection
// base03 #8f7550  comments, invisibles
// base04 #a88c62  dark foreground
// base05 #c3a983  default foreground
// base06 #dec8a7  light foreground
// base08 #a88c62  variables, keywords
// base09 #dec8a7  numbers, constants
// base0A #dec8a7  classes, types
// base0B #dec8a7  strings
// base0C #dec8a7  regex, escape chars
// base0D #c3a983  functions
// base0E #a88c62  keywords, storage
// base0F #876e48  deprecated, punctuation

pub fn token_color(kind: TokenKind) -> Color {
    match kind {
        TokenKind::Default => Color::hex(0xc3a983), // base05
        TokenKind::Comment => Color::hex(0x8f7550), // base03
        TokenKind::String  => Color::hex(0xdec8a7), // base0B
        TokenKind::Number  => Color::hex(0xdec8a7), // base09
        TokenKind::Keyword => Color::hex(0xa88c62), // base0E
        TokenKind::Type    => Color::hex(0xdec8a7), // base0A
        TokenKind::Punct   => Color::hex(0x876e48), // base0F
        TokenKind::Macro   => Color::hex(0xc3a983), // base0D
    }
}

static KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
    "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    "async", "await", "dyn",
];

static TYPES: &[&str] = &[
    "bool", "char", "f32", "f64",
    "i8",  "i16", "i32", "i64", "i128", "isize",
    "u8",  "u16", "u32", "u64", "u128", "usize",
    "str", "String", "Option", "Result", "Vec", "Box", "Arc", "Rc",
    "Some", "None", "Ok", "Err",
];

pub fn lex(src: &str, out: &mut Vec<Token>) {
    out.clear();
    lex_from(src, 0, out);
}

pub fn lex_from(src: &str, byte_offset: usize, out: &mut Vec<Token>) {
    out.clear();

    let bytes = src.as_bytes();
    let len   = bytes.len();
    let mut i = 0usize;

    macro_rules! push {
        ($kind:expr, $start:expr, $end:expr) => {
            if $end > $start {
                out.push(Token::new($kind, ($start + byte_offset) as _, ($end - $start) as _));
            }
        }
    }

    while i < len {
        let start = i;
        let b = bytes[i];

        // Line comment
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            push!(TokenKind::Comment, start, i);
            continue;
        }

        // Block comment (not nested for now)
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len { i += 2; }
            push!(TokenKind::Comment, start, i);
            continue;
        }

        // String literal (double quote)
        if b == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < len { i += 1; }
                    continue;
                }
                if bytes[i] == b'"'  { i += 1; break; }
                i += 1;
            }
            push!(TokenKind::String, start, i);
            continue;
        }

        // Char literal
        if b == b'\'' {
            // Rust lifetimes: 'a, 'static, 'lifetime_name
            // A lifetime is ' followed by a letter/underscore and then
            // an identifier character or end-of-token (space, comma, >, etc.)
            // A char literal is ' followed by any char and then a closing '
            // (or a backslash escape and then closing ')
            //
            // Heuristic: if the byte after ' is alphabetic/underscore AND
            // there is no closing ' within 4 bytes, treat it as a lifetime.
            let is_lifetime = {
                let next = bytes.get(i + 1).copied().unwrap_or(0);
                let after_next = bytes.get(i + 2).copied().unwrap_or(0);
                (next.is_ascii_alphabetic() || next == b'_')
                    && after_next != b'\''  // 'x' is a char literal, 'ab... is a lifetime
            };

            if is_lifetime {
                // Scan the lifetime identifier
                i += 1;
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                push!(TokenKind::Default, start, i);
                continue;
            }

            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < len { i += 1; }
                    continue;
                }
                if bytes[i] == b'\'' { i += 1; break; }
                i += 1;
            }
            push!(TokenKind::String, start, i);
            continue;
        }

        // Number (including hex 0x, floats, suffixes)
        if b.is_ascii_digit() || (b == b'-' && i + 1 < len && bytes[i + 1].is_ascii_digit()) {
            if b == b'-' { i += 1; }
            // hex
            if i + 1 < len && bytes[i] == b'0' && bytes[i + 1] == b'x' {
                i += 2;
                while i < len && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_') { i += 1; }
            } else {
                while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') { i += 1; }
                if i < len && bytes[i] == b'.' && i + 1 < len && bytes[i+1].is_ascii_digit() {
                    i += 1;
                    while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') { i += 1; }
                }
            }
            // optional type suffix: u32, f64, etc.
            if i < len && bytes[i].is_ascii_alphabetic() {
                while i < len && bytes[i].is_ascii_alphanumeric() { i += 1; }
            }
            push!(TokenKind::Number, start, i);
            continue;
        }

        // Identifier, keyword, type, or macro
        if b.is_ascii_alphabetic() || b == b'_' {
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            // Macro: identifier immediately followed by '!'
            if i < len && bytes[i] == b'!' {
                i += 1;
                push!(TokenKind::Macro, start, i);
                continue;
            }
            let word = &src[start..i];
            let kind = if KEYWORDS.contains(&word) {
                TokenKind::Keyword
            } else if TYPES.contains(&word) || (b.is_ascii_uppercase() && word.len() > 1) {
                // PascalCase = type/variant heuristic
                TokenKind::Type
            } else {
                TokenKind::Default
            };
            push!(kind, start, i);
            continue;
        }

        // Whitespace - skip without emitting a token
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            i += 1;
            continue;
        }

        // Everything else: punctuation, one byte at a time
        i += 1;
        push!(TokenKind::Punct, start, i);
    }
}
