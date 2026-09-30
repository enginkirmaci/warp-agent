//! Lightweight syntax highlighter — a direct port of MarkdownTextBlock's
//! HighlightLine: keywords, strings, comments, numbers across the usual
//! language families. Good enough for chat code blocks, zero dependencies.

#[derive(Clone, Copy, PartialEq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Str,
    Comment,
    Num,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Generic,
    Hash,   // '#' line comments: python, shell, yaml…
    Sql,    // '--' line comments
    Markup, // <tags>
}

fn c_like_words(word: &str) -> bool {
    matches!(
        word,
        "abstract" | "alignas" | "and" | "as" | "asm" | "assert" | "async" | "await" | "base" | "bool"
            | "break" | "byte" | "case" | "catch" | "chan" | "char" | "checked" | "class" | "const"
            | "constexpr" | "continue" | "crate" | "decimal" | "default" | "defer" | "delegate" | "delete"
            | "do" | "double" | "dyn" | "dynamic" | "else" | "enum" | "event" | "explicit" | "export"
            | "extends" | "extern" | "false" | "final" | "finally" | "fixed" | "float" | "fn" | "for"
            | "foreach" | "friend" | "from" | "func" | "function" | "get" | "go" | "goto" | "if" | "impl"
            | "implements" | "import" | "in" | "inline" | "instanceof" | "int" | "interface" | "internal"
            | "is" | "let" | "lock" | "long" | "loop" | "match" | "mod" | "move" | "mut" | "namespace"
            | "new" | "nil" | "noexcept" | "not" | "null" | "nullptr" | "object" | "operator" | "or"
            | "out" | "override" | "package" | "params" | "private" | "protected" | "pub" | "public"
            | "readonly" | "ref" | "register" | "return" | "sbyte" | "sealed" | "select" | "self" | "Self"
            | "set" | "short" | "signed" | "sizeof" | "static" | "stackalloc" | "string" | "struct"
            | "super" | "switch" | "synchronized" | "template" | "this" | "throw" | "throws" | "trait"
            | "true" | "try" | "type" | "typedef" | "typeof" | "uint" | "ulong" | "unchecked" | "union"
            | "unsafe" | "unsigned" | "use" | "ushort" | "using" | "var" | "virtual" | "void" | "volatile"
            | "when" | "where" | "while" | "with" | "yield"
    )
}

fn python_words(word: &str) -> bool {
    matches!(
        word,
        "and" | "as" | "assert" | "async" | "await" | "class" | "def" | "del" | "elif" | "else"
            | "except" | "False" | "finally" | "for" | "from" | "global" | "if" | "import" | "in"
            | "is" | "lambda" | "None" | "nonlocal" | "not" | "or" | "pass" | "raise" | "return"
            | "self" | "True" | "try" | "while" | "with" | "yield"
    )
}

fn shell_words(word: &str) -> bool {
    matches!(
        word,
        "alias" | "break" | "case" | "cd" | "continue" | "do" | "done" | "echo" | "elif" | "else"
            | "esac" | "eval" | "exec" | "exit" | "export" | "fi" | "for" | "function" | "if" | "in"
            | "local" | "read" | "readonly" | "return" | "select" | "set" | "shift" | "source"
            | "then" | "trap" | "unset" | "until" | "while"
    )
}

fn sql_words(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "add" | "all" | "alter" | "and" | "as" | "asc" | "begin" | "between" | "by" | "case"
            | "column" | "commit" | "constraint" | "create" | "cross" | "default" | "delete" | "desc"
            | "distinct" | "drop" | "else" | "end" | "exists" | "foreign" | "from" | "full" | "group"
            | "having" | "in" | "index" | "inner" | "insert" | "into" | "is" | "join" | "key" | "left"
            | "like" | "limit" | "not" | "null" | "offset" | "on" | "or" | "order" | "outer" | "primary"
            | "references" | "rollback" | "select" | "set" | "table" | "then" | "union" | "unique"
            | "update" | "values" | "view" | "when" | "where"
    )
}

fn json_words(word: &str) -> bool {
    matches!(word, "true" | "false" | "null")
}

fn lang_info(lang: &str) -> (Option<u8 /*0 c-like, 1 python, 2 shell, 3 sql, 4 json*/>, Mode) {
    match lang {
        "sql" => (Some(3), Mode::Sql),
        "json" => (Some(4), Mode::Generic),
        "xml" | "html" | "xaml" | "svg" | "vue" | "jsx" | "tsx" => (None, Mode::Markup),
        "py" | "python" | "ruby" | "rb" | "yaml" | "yml" | "toml" | "r" | "perl" => (Some(1), Mode::Hash),
        "sh" | "bash" | "shell" | "zsh" | "fish" | "console" | "ps1" | "powershell" | "dockerfile" | "makefile" => {
            (Some(2), Mode::Hash)
        }
        _ => (Some(0), Mode::Generic),
    }
}

fn is_word(word: &str, set: Option<u8>) -> bool {
    match set {
        None => false,
        Some(0) => c_like_words(word),
        Some(1) => python_words(word),
        Some(2) => shell_words(word),
        Some(3) => sql_words(word),
        Some(4) => json_words(word),
        _ => false,
    }
}

fn only_space_before(line: &str, i: usize) -> bool {
    line[..i].trim().is_empty()
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenizes the whole code block (newlines are plain text tokens).
pub fn highlight(code: &str, lang: &str) -> Vec<(String, TokenKind)> {
    let mut tokens: Vec<(String, TokenKind)> = Vec::new();
    let (set, mode) = lang_info(lang);
    let mut in_block_comment = false;
    for (li, raw_line) in code.replace('\r', "").split('\n').enumerate() {
        if li > 0 {
            tokens.push(("\n".to_string(), TokenKind::Plain));
        }
        highlight_line(raw_line, set, mode, &mut in_block_comment, &mut tokens);
    }
    tokens
}

fn highlight_line(line: &str, set: Option<u8>, mode: Mode, in_block_comment: &mut bool, into: &mut Vec<(String, TokenKind)>) {
    let mut plain = String::new();
    let mut i = 0;
    let chars: Vec<char> = line.chars().collect();
    // work on char indices but slice by bytes for token text
    let mut byte_of: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    {
        let mut acc = 0;
        for c in &chars {
            byte_of.push(acc);
            acc += c.len_utf8();
        }
        byte_of.push(acc);
    }
    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                into.push((plain.clone(), TokenKind::Plain));
                plain.clear();
            }
        };
    }

    while i < chars.len() {
        if *in_block_comment {
            match find_from(&chars, i, &['*', '/']) {
                Some(end) if end + 1 < chars.len() => {
                    into.push((chars[i..end + 2].iter().collect(), TokenKind::Comment));
                    i = end + 2;
                    *in_block_comment = false;
                    continue;
                }
                _ => {
                    into.push((chars[i..].iter().collect(), TokenKind::Comment));
                    return;
                }
            }
        }

        let ch = chars[i];

        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' && mode != Mode::Markup {
            flush!();
            match find_from(&chars, i + 2, &['*', '/']) {
                Some(end) if end + 1 < chars.len() => {
                    into.push((chars[i..end + 2].iter().collect(), TokenKind::Comment));
                    i = end + 2;
                }
                _ => {
                    *in_block_comment = true;
                    into.push((chars[i..].iter().collect(), TokenKind::Comment));
                    return;
                }
            }
            continue;
        }

        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' && mode == Mode::Generic {
            flush!();
            into.push((chars[i..].iter().collect(), TokenKind::Comment));
            return;
        }

        if ch == '#' && mode == Mode::Hash && (i == 0 || only_space_before(line, byte_of[i])) {
            flush!();
            into.push((chars[i..].iter().collect(), TokenKind::Comment));
            return;
        }

        if mode == Mode::Sql && ch == '-' && i + 1 < chars.len() && chars[i + 1] == '-' && (i == 0 || only_space_before(line, byte_of[i])) {
            flush!();
            into.push((chars[i..].iter().collect(), TokenKind::Comment));
            return;
        }

        if ch == '"' || ch == '\'' || ch == '`' {
            flush!();
            let end = find_string_end(&chars, i + 1, ch);
            into.push((chars[i..end].iter().collect(), TokenKind::Str));
            i = end;
            continue;
        }

        if mode == Mode::Markup && ch == '<' {
            flush!();
            let mut j = i + 1;
            if j < chars.len() && chars[j] == '/' {
                j += 1;
            }
            while j < chars.len() && (chars[j].is_alphanumeric() || matches!(chars[j], ':' | '-' | '_' | '!')) {
                j += 1;
            }
            into.push((chars[i..j].iter().collect(), TokenKind::Keyword));
            i = j;
            continue;
        }

        if ch.is_ascii_digit() && (i == 0 || !is_ident_char(chars[i - 1])) {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '.') {
                j += 1;
            }
            flush!();
            into.push((chars[i..j].iter().collect(), TokenKind::Num));
            i = j;
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let mut j = i + 1;
            while j < chars.len() && is_ident_char(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            flush!();
            let kind = if is_word(&word, set) { TokenKind::Keyword } else { TokenKind::Plain };
            into.push((word, kind));
            i = j;
            continue;
        }

        plain.push(ch);
        i += 1;
    }
    flush!();
}

fn find_from(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    if needle.len() == 2 {
        (start..chars.len().saturating_sub(1)).find(|&i| chars[i] == needle[0] && chars[i + 1] == needle[1])
    } else {
        (start..chars.len()).find(|&i| chars[i] == needle[0])
    }
}

fn find_string_end(chars: &[char], start: usize, quote: char) -> usize {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '\\' && quote != '`' {
            i += 1;
        } else if chars[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}
