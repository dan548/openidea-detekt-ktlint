//! Text edits that add a rule to `@Suppress` — of the declaration around a
//! line, or of the whole file (`@file:Suppress`). detekt and ktlint both honour
//! the annotation on any enclosing declaration. Kotlin's `Suppress` is not
//! repeatable, so an existing one is extended rather than repeated.
//!
//! This reads the text line by line, not a syntax tree: strings and `//`
//! comments are ignored when counting brackets; a declaration is a line whose
//! first word after annotations and modifiers is a declaration keyword.

use openidea_plugin::{Position, Range, TextEdit};

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "open",
    "final",
    "abstract",
    "sealed",
    "data",
    "enum",
    "annotation",
    "inner",
    "value",
    "inline",
    "override",
    "lateinit",
    "const",
    "suspend",
    "tailrec",
    "operator",
    "infix",
    "external",
    "expect",
    "actual",
    "companion",
    "vararg",
    "noinline",
    "crossinline",
];
const KEYWORDS: &[&str] = &["fun", "class", "interface", "object", "val", "var", "typealias", "constructor"];
const SUPPRESS: &[&str] = &["@Suppress(", "@kotlin.Suppress("];
const FILE_SUPPRESS: &[&str] = &["@file:Suppress(", "@file:kotlin.Suppress("];

/// What adding a rule to a `@Suppress` comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Suppression {
    Edit(TextEdit),
    /// The annotation already names it.
    Already,
}

/// A declaration that can carry `@Suppress`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// 0-based.
    pub line: u32,
    /// `fun`, `class`, … (`fun interface` is `interface`).
    pub keyword: &'static str,
    /// `foo`, `String.foo`; empty when anonymous (`object :`, destructuring).
    pub name: String,
}

fn utf16(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

fn lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The line without string and character literals and without a `//` comment.
fn code(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '/' && chars.peek() == Some(&'/') => break,
            None => out.push(c),
        }
    }
    out
}

/// Brackets the line opens minus those it closes.
fn balance(line: &str) -> i32 {
    code(line)
        .chars()
        .map(|c| match c {
            '{' | '(' | '[' => 1,
            '}' | ')' | ']' => -1,
            _ => 0,
        })
        .sum()
}

fn skip_annotations(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix('@') else { return s };
        let name_len =
            rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | ':'))).unwrap_or(rest.len());
        let mut rest = &rest[name_len..];
        if rest.starts_with('(') {
            let mut depth = 0;
            let mut end = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = &rest[end..];
        }
        s = rest;
    }
}

fn first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// The declaration `line` starts, if it starts one.
pub fn declaration(line: &str) -> Option<(&'static str, String)> {
    let mut rest = skip_annotations(&code(line)).to_owned();
    loop {
        let (word, after) = first_word(&rest);
        if MODIFIERS.contains(&word) {
            rest = after.to_owned();
            continue;
        }
        let mut keyword = KEYWORDS.iter().copied().find(|k| *k == word)?;
        let mut after = after.trim_start();
        if keyword == "fun" && first_word(after).0 == "interface" {
            keyword = "interface";
            after = first_word(after).1.trim_start();
        }
        // Type parameters: `fun <T> foo`.
        if let Some(r) = after.strip_prefix('<') {
            after = r.find('>').map_or("", |i| r[i + 1..].trim_start());
        }
        let end = after.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '`'))).unwrap_or(after.len());
        return Some((keyword, after[..end].trim_end_matches('.').to_owned()));
    }
}

/// The nearest declaration that contains `line`: the line itself, one whose
/// brackets are still open there, or one that continues there more indented.
pub fn enclosing(text: &str, line: u32) -> Option<Declaration> {
    let lines = lines(text);
    let l = (line as usize).min(lines.len().checked_sub(1)?);
    for k in (0..=l).rev() {
        let Some((keyword, name)) = declaration(lines[k]) else { continue };
        let found = Declaration { line: k as u32, keyword, name };
        if k == l {
            return Some(found);
        }
        let mut b = 0;
        if lines[k..l].iter().all(|x| {
            b += balance(x);
            b > 0
        }) {
            return Some(found);
        }
        let own = indent(lines[k]).len();
        if balance(lines[k]) == 0
            && lines[k + 1..=l].iter().filter(|x| !x.trim().is_empty()).all(|x| indent(x).len() > own)
        {
            return Some(found);
        }
    }
    None
}

/// Adds `id` to the `@Suppress(` that starts at byte `at` of line `m`.
fn extend(lines: &[&str], m: usize, at: usize, prefix: &str, id: &str) -> Suppression {
    let quoted = format!("\"{id}\"");
    let open = at + prefix.len(); // just after `(`
    let mut args = String::new();
    'scan: for (i, line) in lines.iter().enumerate().skip(m) {
        let from = if i == m { open } else { 0 };
        for c in line[from..].chars() {
            if c == ')' {
                break 'scan;
            }
            args.push(c);
        }
        args.push('\n');
    }
    if args.contains(&quoted) {
        return Suppression::Already;
    }
    let pos = Position::new(m as u32, utf16(&lines[m][..open]));
    let text = if args.trim().is_empty() { quoted } else { format!("{quoted}, ") };
    Suppression::Edit(TextEdit { range: Range::new(pos, pos), text })
}

/// Finds one of `prefixes` in the code of `line`: (byte, prefix).
fn find_prefix(line: &str, prefixes: &[&'static str]) -> Option<(usize, &'static str)> {
    let code = code(line);
    // `code` drops literals, so it only confirms the annotation; its place is the line's.
    prefixes.iter().find(|p| code.contains(**p)).and_then(|p| line.find(p).map(|i| (i, *p)))
}

/// Suppresses `id` on the declaration `decl`.
pub fn for_declaration(text: &str, decl: &Declaration, id: &str) -> Suppression {
    let lines = lines(text);
    let k = decl.line as usize;
    // The annotations above the declaration, and the declaration's line.
    // An annotation's arguments may span lines: `depth` counts the brackets
    // closed below the line read, so the lines inside them belong to it too.
    let mut first = k;
    let mut depth = 0;
    while first > 0 {
        let t = lines[first - 1].trim_start();
        if !(depth > 0 || t.starts_with(')') || (t.starts_with('@') && !t.starts_with("@file:"))) {
            break;
        }
        depth -= balance(lines[first - 1]);
        if depth < 0 {
            break;
        }
        first -= 1;
    }
    for m in first..=k {
        if let Some((at, prefix)) = find_prefix(lines[m], SUPPRESS) {
            return extend(&lines, m, at, prefix, id);
        }
    }
    let pos = Position::new(decl.line, 0);
    let text = format!("{}@Suppress(\"{id}\"){}", indent(lines[k]), newline(text));
    Suppression::Edit(TextEdit { range: Range::new(pos, pos), text })
}

/// Suppresses `id` in the whole file: `@file:Suppress` before `package`.
pub fn for_file(text: &str, id: &str) -> Suppression {
    let lines = lines(text);
    let nl = newline(text);
    let mut in_comment = false;
    let mut insert = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("*/");
            continue;
        }
        if t.is_empty() || t.starts_with("//") || (i == 0 && t.starts_with("#!")) {
            continue;
        }
        if let Some(rest) = t.strip_prefix("/*") {
            in_comment = !rest.contains("*/");
            continue;
        }
        if t.starts_with("@file:") {
            if let Some((at, prefix)) = find_prefix(line, FILE_SUPPRESS) {
                return extend(&lines, i, at, prefix, id);
            }
            continue;
        }
        insert = i;
        break;
    }
    let annotation = format!("@file:Suppress(\"{id}\"){nl}");
    let (pos, text) = match lines.get(insert) {
        Some(l) if l.trim_start().starts_with("package") || l.trim_start().starts_with("import") => {
            (Position::new(insert as u32, 0), format!("{annotation}{nl}"))
        }
        Some(_) => (Position::new(insert as u32, 0), annotation),
        // Only comments: after the last line.
        None => {
            let last = lines.len() as u32 - 1;
            let end = utf16(lines[last as usize]);
            let lead = if end > 0 { nl } else { "" };
            (Position::new(last, end), format!("{lead}{}", annotation.trim_end()))
        }
    };
    Suppression::Edit(TextEdit { range: Range::new(pos, pos), text })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies one insertion (the only kind made here).
    fn apply(text: &str, s: Suppression) -> String {
        let Suppression::Edit(e) = s else { panic!("nothing to edit") };
        assert_eq!(e.range.start, e.range.end);
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let line = &mut lines[e.range.start.line as usize];
        let at: usize = line.chars().take(e.range.start.character as usize).map(char::len_utf8).sum();
        line.insert_str(at, &e.text);
        lines.join("\n")
    }

    const SRC: &str = "\
package demo

class Shop(val tax: Int = 20) {
    val fee = 7
    init {
        println(42)
    }

    @Test
    fun total(
        items: List<Int>,
    ): Int {
        val list = items.map {
            it * 3
        }
        return list.sum() +
            100
    }
}
";

    fn at(line: u32) -> Declaration {
        enclosing(SRC, line).unwrap()
    }

    #[test]
    fn finds_the_enclosing_declaration() {
        assert_eq!((at(2).keyword, at(2).name.as_str()), ("class", "Shop"));
        assert_eq!(at(3).name, "fee");
        assert_eq!(at(5).name, "Shop", "init is not annotatable; a sibling property is not it");
        assert_eq!(at(10).name, "total", "a parameter list");
        assert_eq!(at(13).name, "list", "a lambda in the initializer");
        assert_eq!(at(15).name, "total", "after the closed lambda");
        assert_eq!(at(16).name, "total", "a continued return");
        assert_eq!(enclosing("// nothing\n", 0), None);
        assert_eq!(declaration("    private suspend fun <T> String.go(x: T)"), Some(("fun", "String.go".into())));
        assert_eq!(declaration("@JvmStatic fun interface Fn {"), Some(("interface", "Fn".into())));
        assert_eq!(declaration("value class Id(val v: Int)"), Some(("class", "Id".into())));
        assert_eq!(declaration("variable = 1"), None);
        assert_eq!(declaration("// fun commented()"), None);
    }

    #[test]
    fn annotates_the_declaration() {
        let out = apply(SRC, for_declaration(SRC, &at(13), "MagicNumber"));
        assert!(out.contains("        @Suppress(\"MagicNumber\")\n        val list = items.map {"), "{out}");
    }

    #[test]
    fn extends_an_existing_suppress() {
        let src = "@Test\n@Suppress(\"A\") // why\nfun f() = 1\n";
        let d = enclosing(src, 2).unwrap();
        assert_eq!(apply(src, for_declaration(src, &d, "B")), "@Test\n@Suppress(\"B\", \"A\") // why\nfun f() = 1\n");
        let empty = "@Suppress() fun f() = 1";
        let d = enclosing(empty, 0).unwrap();
        assert_eq!(apply(empty, for_declaration(empty, &d, "B")), "@Suppress(\"B\") fun f() = 1");
        assert_eq!(for_declaration(src, &enclosing(src, 2).unwrap(), "A"), Suppression::Already);
        let multi = "@Suppress(\n    \"A\",\n)\nfun f() = 1";
        let d = enclosing(multi, 3).unwrap();
        assert_eq!(for_declaration(multi, &d, "A"), Suppression::Already);
    }

    #[test]
    fn suppresses_in_the_file() {
        let src = "/*\n * License\n */\n\npackage demo\n\nfun f() = 1\n";
        assert_eq!(
            apply(src, for_file(src, "ktlint:standard:filename")),
            "/*\n * License\n */\n\n@file:Suppress(\"ktlint:standard:filename\")\n\npackage demo\n\nfun f() = 1\n"
        );
        let has = "@file:Suppress(\"A\")\n\npackage demo\n";
        assert_eq!(apply(has, for_file(has, "B")), "@file:Suppress(\"B\", \"A\")\n\npackage demo\n");
        assert_eq!(for_file(has, "A"), Suppression::Already);
        let script = "#!/usr/bin/env kotlin\nprintln(1)\n";
        assert_eq!(apply(script, for_file(script, "X")), "#!/usr/bin/env kotlin\n@file:Suppress(\"X\")\nprintln(1)\n");
        assert_eq!(apply("// only", for_file("// only", "X")), "// only\n@file:Suppress(\"X\")");
    }

    #[test]
    fn keeps_crlf_and_ignores_strings() {
        let src = "class A {\r\n    val s = \"{ not a block\"\r\n    fun f() = 1\r\n}\r\n";
        assert_eq!(enclosing(src, 2).unwrap().name, "f");
        assert_eq!(enclosing(src, 1).unwrap().name, "s");
        let Suppression::Edit(e) = for_declaration(src, &enclosing(src, 2).unwrap(), "X") else { panic!() };
        assert_eq!(e.text, "    @Suppress(\"X\")\r\n");
    }
}
