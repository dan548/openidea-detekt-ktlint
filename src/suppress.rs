//! Text edits that add a rule to `@Suppress` — of the declaration around a
//! line, or of the whole file (`@file:Suppress`). detekt and ktlint both honour
//! the annotation on any enclosing declaration. Kotlin's `Suppress` is not
//! repeatable, so an existing one is extended rather than repeated.
//!
//! Declaration detection uses bracket balance and indentation. It skips comments
//! and literals before looking for declaration keywords and annotations.

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

/// Returns the text after a comment or literal at the start of `text`.
fn after_non_code(text: &str) -> Option<&str> {
    if text.starts_with("//") {
        return Some(text.find('\n').map_or("", |end| &text[end..]));
    }
    if let Some(mut rest) = text.strip_prefix("/*") {
        let mut depth = 1;
        while let Some(at) = rest.find(['/', '*']) {
            rest = &rest[at..];
            if let Some(tail) = rest.strip_prefix("/*") {
                depth += 1;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("*/") {
                depth -= 1;
                if depth == 0 {
                    return Some(tail);
                }
                rest = tail;
            } else {
                rest = &rest[1..];
            }
        }
        return Some("");
    }
    if let Some(rest) = text.strip_prefix("\"\"\"") {
        return Some(rest.find("\"\"\"").map_or("", |end| rest[end + 3..].trim_start_matches('"')));
    }
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let rest = &text[1..];
    let mut chars = rest.char_indices();
    while let Some((at, c)) = chars.next() {
        if c == '\n' {
            return Some(&rest[at..]);
        }
        if c == '\\' {
            chars.next();
        } else if c == quote {
            return Some(&rest[at + 1..]);
        }
    }
    Some("")
}

/// Masks comments and literals while preserving line breaks and byte positions.
fn code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(tail) = after_non_code(rest) {
            out.extend(rest[..rest.len() - tail.len()].bytes().map(|b| match b {
                b'\r' | b'\n' => char::from(b),
                _ => ' ',
            }));
            rest = tail;
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

fn balance(line: &str) -> i32 {
    line.chars()
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
fn declaration(line: &str) -> Option<(&'static str, String)> {
    let mut rest = skip_annotations(line);
    loop {
        let (word, after) = first_word(rest);
        if MODIFIERS.contains(&word) {
            rest = after;
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
    let l = line as usize;
    lines.get(l)?;
    let code = code(text);
    let visible = self::lines(&code);
    let balances: Vec<_> = visible[..=l].iter().map(|line| balance(line)).collect();
    for k in (0..=l).rev() {
        // Inserting at the start of a line that begins inside a comment or literal is unsafe.
        if indent(visible[k]).len() != indent(lines[k]).len() {
            continue;
        }
        let Some((keyword, name)) = declaration(visible[k]) else { continue };
        let found = Declaration { line: k as u32, keyword, name };
        if k == l {
            return Some(found);
        }
        let mut b = 0;
        if balances[k..l].iter().all(|x| {
            b += x;
            b > 0
        }) {
            return Some(found);
        }
        let own = indent(lines[k]).len();
        if balances[k] == 0
            && !visible[l].trim().is_empty()
            && visible[k + 1..=l].iter().filter(|x| !x.trim().is_empty()).all(|x| indent(x).len() > own)
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
    prefixes.iter().find_map(|p| line.find(p).map(|i| (i, *p)))
}

/// Suppresses `id` on the declaration `decl`.
pub fn for_declaration(text: &str, decl: &Declaration, id: &str) -> Suppression {
    let lines = lines(text);
    let k = decl.line as usize;
    let code = code(text);
    let visible = self::lines(&code);
    // The annotations above the declaration, and the declaration's line.
    // An annotation's arguments may span lines: `depth` counts the brackets
    // closed below the line read, so the lines inside them belong to it too.
    let mut first = k;
    let mut depth = 0;
    while first > 0 {
        let t = visible[first - 1].trim_start();
        if !(depth > 0 || t.starts_with(')') || (t.starts_with('@') && !t.starts_with("@file:"))) {
            break;
        }
        depth -= balance(visible[first - 1]);
        if depth < 0 {
            break;
        }
        first -= 1;
    }
    for (m, line) in visible.iter().enumerate().take(k + 1).skip(first) {
        if let Some((at, prefix)) = find_prefix(line, SUPPRESS) {
            return extend(&lines, m, at, prefix, id);
        }
    }
    let pos = Position::new(decl.line, 0);
    let text = format!("{}@Suppress(\"{id}\"){}", indent(lines[k]), newline(text));
    Suppression::Edit(TextEdit { range: Range::new(pos, pos), text })
}

/// Counts the extra lines occupied by a file annotation's argument list.
fn file_annotation_lines(annotation: &str) -> usize {
    let name_len =
        annotation.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.'))).unwrap_or(annotation.len());
    let after_name = &annotation[name_len..];
    let args = after_name.trim_start();
    let Some((open, close)) = args.chars().next().and_then(|c| match c {
        '(' => Some(('(', ')')),
        '[' => Some(('[', ']')),
        _ => None,
    }) else {
        return 0;
    };

    let mut rest = args;
    let mut depth = 0;
    while let Some(c) = rest.chars().next() {
        if let Some(tail) = after_non_code(rest) {
            rest = tail;
            continue;
        }
        rest = &rest[c.len_utf8()..];
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
    }
    after_name[..after_name.len() - rest.len()].bytes().filter(|&b| b == b'\n').count()
}

/// Suppresses `id` in the whole file: `@file:Suppress` before `package`.
pub fn for_file(text: &str, id: &str) -> Suppression {
    let lines = lines(text);
    let nl = newline(text);
    let mut in_comment = false;
    let mut insert = lines.len();
    let mut annotation_end = 0;
    let mut offset = 0;
    for (i, raw) in text.split('\n').enumerate() {
        let start = offset;
        offset += raw.len() + 1;
        if i < annotation_end {
            continue;
        }
        let line = lines[i];
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
        if let Some(annotation) = t.strip_prefix("@file:") {
            if let Some((at, prefix)) = find_prefix(&code(line), FILE_SUPPRESS) {
                return extend(&lines, i, at, prefix, id);
            }
            let annotation_start = start + indent(line).len() + "@file:".len();
            debug_assert!(text[annotation_start..].starts_with(annotation));
            annotation_end = i + 1 + file_annotation_lines(&text[annotation_start..]);
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
        assert_eq!(declaration(&code("// fun commented()")), None);
    }

    #[test]
    fn annotates_the_declaration() {
        let out = apply(SRC, for_declaration(SRC, &at(13), "MagicNumber"));
        assert!(out.contains("        @Suppress(\"MagicNumber\")\n        val list = items.map {"), "{out}");
    }

    #[test]
    fn ignores_declarations_inside_comments_and_raw_strings() {
        for comment_or_literal in [
            "    /* outer\n        /* nested */\n        fun fake() {\n    */\n",
            "    val template = \"\"\"\n        fun fake() {\n    \"\"\"\n",
        ] {
            let src = format!("fun actual() {{\n{comment_or_literal}    println(42)\n}}\n");
            let finding_line = 1 + comment_or_literal.lines().count() as u32;
            let decl = enclosing(&src, finding_line).unwrap();
            assert_eq!((decl.line, decl.name.as_str()), (0, "actual"));
            assert!(
                apply(&src, for_declaration(&src, &decl, "MagicNumber"))
                    .starts_with("@Suppress(\"MagicNumber\")\nfun actual()")
            );
        }
        assert_eq!(enclosing("/*\nfun fake() {\n*/\nprintln(42)\n", 3), None);
        assert_eq!(enclosing("/*\n*/ fun actual() = 42\n", 1), None);
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
