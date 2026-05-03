use std::path::MAIN_SEPARATOR;

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
    /// Max token length: 16,777,215 bytes.
    pub kind_len: u32,
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

// @Incomplete: Highlight :Notes and @Notes and potentially stuff like TODO, IMPORTANT, etc

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

pub const fn token_color(kind: TokenKind) -> Color {
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

const C_WHITESPACE: u8 = 0;
const C_ALPHA:      u8 = 1; // a-z, A-Z, _
const C_DIGIT:      u8 = 2; // 0-9
const C_SLASH:      u8 = 3; // /
const C_QUOTE:      u8 = 4; // "
const C_TICK:       u8 = 5; // '
const C_PUNCT:      u8 = 6; // everything else

static CHAR_CLASSES: [u8; 256] = {
    let mut table = [C_PUNCT; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            table[i] = C_WHITESPACE;
        } else if b.is_ascii_alphabetic() || b == b'_' {
            table[i] = C_ALPHA;
        } else if b.is_ascii_digit() {
            table[i] = C_DIGIT;
        } else if b == MAIN_SEPARATOR as u8 {
            table[i] = C_SLASH;
        } else if b == b'"' {
            table[i] = C_QUOTE;
        } else if b == b'\'' {
            table[i] = C_TICK;
        }
        i += 1;
    }
    table
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LexState {
    #[default]
    Normal,
    InString,
    InBlockComment,
    InRustRawString(u8) // Hash count
}

/// Returns the lexer state at end of input (for incremental relex).
/// `byte_offset` shifts all emitted token start positions.
/// Tokens are APPENDED to `out` (caller decides whether to clear).
pub fn lex_from(
    src: &str,
    byte_offset: usize,
    state: LexState,
    out: &mut Vec<Token>,
) -> LexState {
    let bytes = src.as_bytes();
    let len   = bytes.len();
    let mut i = 0usize;
    let mut cur_state = state;

    // Ensure we don't reallocate mid-lex
    out.reserve(len / 4);

    macro_rules! push {
        ($kind:expr, $start:expr, $end:expr) => {
            if $end > $start {
                out.push(Token::new($kind, $start + byte_offset, $end - $start));
            }
        };
    }

    // If we're resuming mid-block-comment, scan until we close it
    if cur_state == LexState::InBlockComment {
        let start = 0;
        while i < len {
            if let Some(next_star) = memchr::memchr(b'*', &bytes[i..]) {
                i += next_star;
                if i + 1 < len && bytes[i + 1] == MAIN_SEPARATOR as u8 {
                    i += 2;
                    push!(TokenKind::Comment, start, i);
                    break;
                }
                i += 1;
            } else {
                i = len;
                // Still in block comment at end of src
                push!(TokenKind::Comment, start, i);
                return LexState::InBlockComment;
            }
        }

        cur_state = LexState::Normal;
    }

    if cur_state == LexState::InString {
        let start = 0;
        let mut closed = false;
        while i < len {
            if let Some(hit) = memchr::memchr2(b'"', b'\\', &bytes[i..]) {
                i += hit;
                if bytes[i] == b'\\' {
                    i += 2; // Skip \ and the next char
                } else {
                    i += 1; // Closing "
                    closed = true;
                    break;
                }
            } else {
                i = len; break;
            }
        }

        push!(TokenKind::String, start, i);
        if !closed { return LexState::InString; }
        cur_state = LexState::Normal;
    }

    if let LexState::InRustRawString(hashes) = cur_state {
        let start = 0;
        loop {
            match memchr::memchr(b'"', &bytes[i..]) {
                None => {
                    i = len;
                    push!(TokenKind::String, start, i);
                    return LexState::InRustRawString(hashes);
                }

                Some(hit) => {
                    i += hit + 1;
                    let mut h = 0u8;
                    while h < hashes && i < len && bytes[i] == b'#' {
                        i += 1; h += 1;
                    }
                    if h == hashes {
                        push!(TokenKind::String, start, i);
                        cur_state = LexState::Normal;
                        break;
                    }
                }
            }
        }
    }

    while i < len {
        let start = i;
        let b = bytes[i];
        let class = CHAR_CLASSES[b as usize];

        match class {
            C_WHITESPACE => {
                // Hot path: Skip whitespace as a block
                i += 1;
                while i < len && CHAR_CLASSES[bytes[i] as usize] == C_WHITESPACE {
                    i += 1;
                }
            }

            C_ALPHA => {
                let mut has_lowercase = false;

                i += 1; // We already know the first char is alpha/underscore
                if b.is_ascii_lowercase() { has_lowercase = true; }

                while i < len {
                    let c = bytes[i];
                    let class = CHAR_CLASSES[c as usize];
                    if class == C_ALPHA || class == C_DIGIT {
                        if c.is_ascii_lowercase() { has_lowercase = true; }
                        i += 1;
                    } else {
                        break;
                    }
                }

                // Check for macro!
                if i < len && bytes[i] == b'!' {
                    i += 1;
                    push!(TokenKind::Macro, start, i);
                } else {
                    let word = &src[start..i];

                    if word == "r" && i < len && (bytes[i] == b'"' || bytes[i] == b'#') {
                        //
                        // Raw string!
                        //

                        // count leading #
                        let mut hashes: u8 = 0;
                        while i < len && bytes[i] == b'#' { i += 1; hashes += 1; }
                        if i < len && bytes[i] == b'"' {
                            i += 1; // "

                            //
                            // Scan for closing " followed by exactly `hashes` #
                            //
                            loop {
                                match memchr::memchr(b'"', &bytes[i..]) {
                                    None => {
                                        i = len;
                                        push!(TokenKind::String, start, i);
                                        return LexState::InRustRawString(hashes);
                                    }
                                    Some(hit) => {
                                        i += hit + 1;
                                        let mut h = 0u8;
                                        while h < hashes && i < len && bytes[i] == b'#' {
                                            i += 1; h += 1;
                                        }
                                        if h == hashes {
                                            push!(TokenKind::String, start, i);
                                            break;  // Closed
                                        }

                                        // Wrong number of #, keep scanning
                                    }
                                }
                            }
                        } else {
                            // Just the identifier `r`, no raw string
                            push!(TokenKind::Default, start, i);
                        }
                    }

                    let mut kind = match word.len() {
                        2 => if matches!(word, "fn" | "if" | "as" | "in" | "do" | "is" | "go" | "to") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        3 => if matches!(word, "for" | "let" | "mut" | "pub" | "use" | "mod" | "try" | "new" | "var" | "def" | "nil") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        4 => if matches!(word, "impl" | "enum" | "type" | "else" | "case" | "char" | "byte" | "void" | "true" | "self" | "goto" | "with") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        5 => if matches!(word, "match" | "const" | "while" | "break" | "async" | "await" | "trait" | "false" | "super" | "final" | "class" | "yield" | "range") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        6 => if matches!(word, "return" | "struct" | "extern" | "import" | "public" | "static" | "switch" | "typeof" | "delete") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        7 => if matches!(word, "default" | "private" | "virtual" | "package" | "extends" | "finally") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        8 => if matches!(word, "continue") {
                            Some(TokenKind::Keyword)
                        } else { None },

                        _ => None,
                    };

                    // If not a specific keyword, apply the heuristic
                    if kind.is_none() {
                        kind = Some(if b == b'_' {
                            if has_lowercase {
                                TokenKind::Default       // snake_case (my_var)
                            } else {
                                TokenKind::Type          // SCREAMING_SNAKE (YARRR, MAX_VAL)
                            }
                        } else if b.is_ascii_uppercase() {
                            TokenKind::Type
                        } else {
                            TokenKind::Default           // snake_case (my_var)
                        });
                    };

                    push!(unsafe { kind.unwrap_unchecked() }, start, i);
                }
            }

            C_DIGIT => {
                i += 1;

                while i < len && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_' || bytes[i] == b'x' || bytes[i] == b'.') {
                    i += 1;
                }

                // Optional suffix
                if i < len && bytes[i].is_ascii_alphabetic() {
                    while i < len && bytes[i].is_ascii_alphanumeric() { i += 1; }
                }

                push!(TokenKind::Number, start, i);
            }

            C_SLASH => {
                if i + 1 < len {
                    if bytes[i+1] == MAIN_SEPARATOR as u8 { // Line comment
                        i = memchr::memchr(b'\n', &bytes[i..]).map_or(len, |pos| i + pos);
                        push!(TokenKind::Comment, start, i);
                    } else if bytes[i+1] == b'*' { // Block comment
                        i += 2;

                        let mut closed = false;
                        while i < len {
                            if let Some(pos) = memchr::memchr(b'*', &bytes[i..]) {
                                i += pos + 1;
                                if i < len && bytes[i] == MAIN_SEPARATOR as u8 {
                                    i += 1;
                                    closed = true;
                                    push!(TokenKind::Comment, start, i);
                                    break;
                                }
                            } else { break; }
                        }

                        if !closed {
                            i = len;
                            push!(TokenKind::Comment, start, i);
                            return LexState::InBlockComment;
                        }
                    } else {
                        i += 1;
                        push!(TokenKind::Punct, start, i);
                    }
                } else {
                    i += 1;
                    push!(TokenKind::Punct, start, i);
                }
            }

            C_QUOTE => {
                // String literal
                i += 1;
                let mut closed = false;
                while i < len {
                    if let Some(hit) = memchr::memchr2(b'"', b'\\', &bytes[i..]) {
                        i += hit;
                        if bytes[i] == b'\\' {
                            i += 2; // Skip \ and the next char
                        } else {
                            i += 1; // Closing "
                            closed = true;
                            break;
                        }
                    } else {
                        i = len; break;
                    }
                }

                push!(TokenKind::String, start, i);
                if !closed { return LexState::InString; }
            }

            C_TICK => {
                // Lifetimes vs Char literals
                let is_lifetime = if i + 1 < len {
                    let next = bytes[i+1];
                    (next.is_ascii_alphabetic() || next == b'_') && bytes.get(i + 2) != Some(&b'\'')
                } else {
                    false
                };

                if is_lifetime {
                    i += 2;
                    while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                        i += 1;
                    }

                    push!(TokenKind::Default, start, i);
                } else {
                    // Char literal logic
                    i += 1;
                    while i < len {
                        if let Some(hit) = memchr::memchr2(b'\'', b'\\', &bytes[i..]) {
                            i += hit;
                            if bytes[i] == b'\\' { i += 2; } else { i += 1; break; }
                        } else {
                            i = len;
                            break;
                        }
                    }

                    push!(TokenKind::String, start, i);
                }
            }

            _ => {
                // Punctuation
                i += 1;
                push!(TokenKind::Punct, start, i);
            }
        }
    }

    cur_state
}
