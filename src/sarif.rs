//! The part of SARIF 2.1.0 that detekt (`--report sarif:<file>`) and ktlint
//! (`--reporter=sarif`) write: one run with results, each at one region of one
//! file.

use openidea_plugin::Severity;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Which tool reported a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    Detekt,
    Ktlint,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Detekt => "detekt",
            Tool::Ktlint => "ktlint",
        }
    }
}

/// One reported problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub tool: Tool,
    /// The file as reported: absolute when the report says where it is relative to.
    pub file: String,
    /// 0-based.
    pub line: u32,
    /// 0-based UTF-16 (SARIF's default `columnKind`); `None` for the whole line.
    pub column: Option<u32>,
    /// 0-based, exclusive.
    pub end: Option<(u32, u32)>,
    pub severity: Severity,
    pub message: String,
    /// `MagicNumber` (detekt), `standard:no-wildcard-imports` (ktlint).
    pub rule: String,
}

impl Finding {
    /// The name `@Suppress` takes for the rule.
    pub fn suppress_id(&self) -> String {
        match self.tool {
            Tool::Detekt => self.rule.clone(),
            Tool::Ktlint => format!("ktlint:{}", self.rule),
        }
    }
}

#[derive(Deserialize)]
struct Log {
    #[serde(default)]
    runs: Vec<Run>,
}

#[derive(Deserialize)]
struct Run {
    #[serde(default)]
    results: Vec<SarifResult>,
    #[serde(rename = "originalUriBaseIds", default)]
    base_ids: BTreeMap<String, ArtifactLocation>,
}

#[derive(Deserialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
    level: Option<String>,
    message: Option<Message>,
    #[serde(default)]
    locations: Vec<Location>,
}

#[derive(Deserialize)]
struct Message {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Location {
    #[serde(rename = "physicalLocation")]
    physical: Option<PhysicalLocation>,
}

#[derive(Deserialize)]
struct PhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact: Option<ArtifactLocation>,
    region: Option<Region>,
}

#[derive(Deserialize)]
struct ArtifactLocation {
    uri: Option<String>,
    #[serde(rename = "uriBaseId")]
    base_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Region {
    start_line: Option<u32>,
    start_column: Option<u32>,
    end_line: Option<u32>,
    end_column: Option<u32>,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `file:` URI as a path (`file:///C:/a` → `C:/a`); anything else decoded as is.
fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file:") else { return percent_decode(uri) };
    let path = match rest.strip_prefix("//") {
        Some(r) if r.starts_with('/') => percent_decode(r),
        Some(r) if r.starts_with("localhost/") => percent_decode(&r["localhost".len()..]),
        // file://server/share → //server/share
        Some(r) => format!("//{}", percent_decode(r)),
        None => percent_decode(rest),
    };
    let b = path.as_bytes();
    if b.len() > 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' { path[1..].to_owned() } else { path }
}

fn resolve(artifact: &ArtifactLocation, bases: &BTreeMap<String, ArtifactLocation>) -> Option<String> {
    let uri = artifact.uri.as_deref()?;
    let path = uri_to_path(uri);
    if uri.starts_with("file:") || path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':') {
        return Some(path);
    }
    match artifact.base_id.as_deref().and_then(|id| bases.get(id)).and_then(|b| b.uri.as_deref()) {
        // ktlint 1.x: relative to the working directory, often with `../..`.
        Some(base) => Some(clean(&format!("{}/{}", uri_to_path(base).trim_end_matches(['/', '\\']), path))),
        None => Some(path),
    }
}

/// `path` without `.` and `..` segments (and with `/` separators).
fn clean(path: &str) -> String {
    let path = path.replace('\\', "/");
    let lead = if path.starts_with("//") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in path[lead.len()..].split('/') {
        match seg {
            "" | "." => {}
            // `C:` stays: `..` never climbs above a drive or the root.
            ".." if parts.last().is_some_and(|p| *p != ".." && !p.ends_with(':')) => {
                parts.pop();
            }
            ".." if !lead.is_empty() || parts.last().is_some_and(|p| p.ends_with(':')) => {}
            _ => parts.push(seg),
        }
    }
    format!("{lead}{}", parts.join("/"))
}

/// The JSON object in `out`: ktlint prints log lines (`… WARN …`) to stdout before it.
fn json_part(out: &str) -> &str {
    let start = out
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .chain([0])
        .filter(|i| out[*i..].starts_with('{'))
        .min()
        .or_else(|| out.find('{'));
    match (start, out.rfind('}')) {
        (Some(a), Some(b)) if a < b => &out[a..=b],
        _ => out,
    }
}

/// The rule's own id from a SARIF `ruleId`.
fn rule_name(tool: Tool, id: &str) -> String {
    match tool {
        // `detekt.style.MagicNumber` (1.x), `style.MagicNumber` / `MagicNumber` (2.x).
        Tool::Detekt => id.rsplit(['.', ':']).next().unwrap_or(id).to_owned(),
        // `standard:indent`; a bare id belongs to the standard rule set.
        Tool::Ktlint if id.contains(':') => id.to_owned(),
        Tool::Ktlint => format!("standard:{id}"),
    }
}

/// The findings of a report, in report order.
pub fn parse(tool: Tool, report: &str) -> Result<Vec<Finding>, String> {
    let log: Log = serde_json::from_str(json_part(report)).map_err(|e| format!("not a SARIF report ({e})"))?;
    let mut out = Vec::new();
    for run in log.runs {
        for r in run.results {
            let severity = match r.level.as_deref() {
                Some("error") => Severity::Error,
                Some("note") => Severity::Info,
                Some("none") => Severity::Hint,
                _ => Severity::Warning,
            };
            let rule = rule_name(tool, r.rule_id.as_deref().unwrap_or(""));
            let message = r.message.and_then(|m| m.text).unwrap_or_else(|| rule.clone());
            for physical in r.locations.into_iter().filter_map(|l| l.physical) {
                let (Some(file), Some(region)) =
                    (physical.artifact.as_ref().and_then(|a| resolve(a, &run.base_ids)), physical.region)
                else {
                    continue;
                };
                let Some(line) = region.start_line.filter(|l| *l > 0) else { continue };
                let column = region.start_column.map(|c| c.saturating_sub(1));
                let end = match (region.end_line, region.end_column) {
                    (Some(l), Some(c)) if l > 0 => Some((l - 1, c.saturating_sub(1))),
                    _ => None,
                };
                out.push(Finding {
                    tool,
                    file,
                    line: line - 1,
                    column,
                    end,
                    severity,
                    message: message.clone(),
                    rule: rule.clone(),
                });
            }
        }
    }
    Ok(out)
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = match path.strip_prefix("//?/UNC/") {
        Some(rest) => format!("//{rest}"),
        None => path.strip_prefix("//?/").unwrap_or(&path).to_owned(),
    };
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}

/// Whether a reported file is `path` (absolute, as the IDE names it).
pub fn same_file(reported: &str, path: &str) -> bool {
    // WASI plugins run on every host: read the host's path, not cfg(windows).
    let windows = path.as_bytes().get(1) == Some(&b':') || path.starts_with("//") || path.starts_with('\\');
    let (reported, path) =
        if windows { (normalize(reported), normalize(path)) } else { (reported.to_owned(), path.to_owned()) };
    let eq = |a: &str, b: &str| if windows { a.eq_ignore_ascii_case(b) } else { a == b };
    if eq(&reported, &path) {
        return true;
    }
    // A relative report path: the end of the absolute one, at a component boundary.
    let rel = reported.strip_prefix("./").unwrap_or(&reported);
    !rel.starts_with('/')
        && path.len() > rel.len()
        && path.is_char_boundary(path.len() - rel.len())
        && eq(&path[path.len() - rel.len()..], rel)
        && path[..path.len() - rel.len()].ends_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETEKT: &str = r#"{
  "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
  "version": "2.1.0",
  "runs": [{
    "tool": {"driver": {"name": "detekt", "rules": []}},
    "results": [
      {"ruleId": "detekt.style.MagicNumber", "level": "warning",
       "message": {"text": "This expression contains a magic number."},
       "locations": [{"physicalLocation": {"artifactLocation": {"uri": "file:///project/src/My%20App.kt"},
         "region": {"startLine": 3, "startColumn": 13, "endLine": 3, "endColumn": 15}}}]},
      {"ruleId": "detekt.naming.FunctionNaming", "level": "error",
       "message": {"text": "Function names should match the pattern."},
       "locations": [{"physicalLocation": {"artifactLocation": {"uri": "src/My App.kt", "uriBaseId": "%SRCROOT%"},
         "region": {"startLine": 2, "startColumn": 5}}}]}
    ],
    "originalUriBaseIds": {"%SRCROOT%": {"uri": "file:///project/"}}
  }]
}"#;

    const KTLINT: &str = r#"WARN some log line
{"version": "2.1.0", "runs": [{
  "tool": {"driver": {"name": "ktlint"}},
  "originalUriBaseIds": {"%SRCROOT%": {"uri": "file:///project/"}},
  "results": [
    {"ruleId": "standard:no-wildcard-imports", "level": "error", "message": {"text": "Wildcard import"},
     "locations": [{"physicalLocation": {"artifactLocation": {"uri": "src/My App.kt", "uriBaseId": "%SRCROOT%"},
       "region": {"startLine": 1, "startColumn": 1}}}]}
  ]
}]}"#;

    #[test]
    fn parses_detekt() {
        let f = parse(Tool::Detekt, DETEKT).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].file, "/project/src/My App.kt");
        assert_eq!((f[0].line, f[0].column, f[0].end), (2, Some(12), Some((2, 14))));
        assert_eq!((f[0].rule.as_str(), f[0].severity), ("MagicNumber", Severity::Warning));
        assert_eq!(f[1].file, "/project/src/My App.kt");
        assert_eq!((f[1].rule.as_str(), f[1].severity, f[1].end), ("FunctionNaming", Severity::Error, None));
        assert_eq!(f[0].suppress_id(), "MagicNumber");
    }

    #[test]
    fn parses_ktlint_around_log_lines() {
        let f = parse(Tool::Ktlint, KTLINT).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].file, "/project/src/My App.kt");
        assert_eq!(f[0].rule, "standard:no-wildcard-imports");
        assert_eq!(f[0].suppress_id(), "ktlint:standard:no-wildcard-imports");
        assert!(parse(Tool::Ktlint, "no report").is_err());
    }

    #[test]
    fn file_uris() {
        assert_eq!(uri_to_path("file:///C:/p/A.kt"), "C:/p/A.kt");
        assert_eq!(uri_to_path("file:/home/a/A.kt"), "/home/a/A.kt");
        assert_eq!(uri_to_path("file://server/share/A.kt"), "//server/share/A.kt");
        assert_eq!(rule_name(Tool::Ktlint, "indent"), "standard:indent");
        assert_eq!(rule_name(Tool::Detekt, "style.MagicNumber"), "MagicNumber");
    }

    #[test]
    fn cleans_joined_paths() {
        assert_eq!(clean("/home/me/../../project/./src/A.kt"), "/project/src/A.kt");
        assert_eq!(clean("/../a"), "/a");
        assert_eq!(clean("C:/a/../../b"), "C:/b");
        assert_eq!(clean("//server/share/x/../A.kt"), "//server/share/A.kt");
        assert_eq!(json_part("WARN {not json}\n{\"runs\": []}\n"), "{\"runs\": []}");
    }

    /// What detekt 1.23.8 and ktlint 1.8.0 wrote for `tests/fixtures/Shop.kt`
    /// (paths moved to /project; ktlint ran in /home/me).
    #[test]
    fn parses_real_reports() {
        let d = parse(Tool::Detekt, include_str!("../tests/fixtures/detekt-1.23.8.sarif")).unwrap();
        let rules: Vec<_> = d.iter().map(|f| (f.rule.as_str(), f.line, f.column, f.end)).collect();
        assert_eq!(
            rules,
            [
                ("InvalidPackageDeclaration", 0, Some(0), Some((0, 12))),
                ("FunctionNaming", 5, Some(8), Some((5, 13))),
                ("MagicNumber", 7, Some(17), Some((7, 19))),
                ("MagicNumber", 9, Some(28), Some((9, 31))),
            ]
        );
        let k = parse(Tool::Ktlint, include_str!("../tests/fixtures/ktlint-1.8.0.sarif")).unwrap();
        assert_eq!(k.len(), 5);
        assert!(k.iter().all(|f| f.file == "/project/src/Shop.kt" && f.end.is_none()), "{k:#?}");
        assert!(k.iter().all(|f| same_file(&f.file, "/project/src/Shop.kt")));
        assert_eq!(
            (k[0].rule.as_str(), k[0].line, k[0].severity),
            ("standard:no-wildcard-imports", 2, Severity::Error)
        );
    }

    #[test]
    fn matches_files() {
        assert!(same_file("/p/src/A.kt", "/p/src/A.kt"));
        assert!(same_file("src/A.kt", "/p/src/A.kt"));
        assert!(same_file("./src/A.kt", "/p/src/A.kt"));
        assert!(!same_file("A.kt", "/p/src/BA.kt"));
        assert!(same_file(r"C:\p\src\A.kt", "C:/p/src/A.kt"));
        assert!(same_file(r"\\?\c:\p\src\A.kt", "C:/p/src/A.kt"));
        assert!(!same_file("/p/A\\B.kt", "/p/A/B.kt"));
    }
}
