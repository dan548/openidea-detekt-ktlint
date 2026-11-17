//! detekt + ktlint for OpenIDEA: runs both command lines on a Kotlin file when
//! it is saved or changed on disk (including quick fixes and undo), as one
//! cancellable task. It reads their SARIF reports, publishes one set of
//! diagnostics and offers quick fixes that suppress a rule with `@Suppress`
//! on the enclosing declaration or `@file:Suppress` for the whole file.
//!
//! detekt uses the project's configuration (`detekt.yml`,
//! `config/detekt/detekt.yml` as the Gradle plugin keeps it, or `.detekt.yml`)
//! and baseline when it has them; ktlint reads the project's `.editorconfig`
//! itself, since both run in the project root. Either tool may be missing or
//! not granted: the other one still runs.

pub mod sarif;
pub mod suppress;

use openidea_plugin::host::{self, fs};
use openidea_plugin::providers::{CodeAction, Document, Providers};
use openidea_plugin::{
    Diagnostic, Error, Event, Invocation, NotificationLevel, Plugin, Position, Project, Range, register_plugin,
};
use sarif::{Finding, Tool};
use std::collections::{BTreeMap, BTreeSet};
use suppress::Suppression;

/// detekt configurations, by preference (project-relative).
const DETEKT_CONFIGS: [&str; 4] = ["detekt.yml", "config/detekt/detekt.yml", "config/detekt.yml", ".detekt.yml"];
const DETEKT_BASELINES: [&str; 3] = ["config/detekt/baseline.xml", "detekt-baseline.xml", "baseline.xml"];

#[derive(Default)]
pub struct KotlinLint {
    /// Last findings per file (for quick fixes; rebuilt after disk changes).
    findings: BTreeMap<String, Vec<Finding>>,
    checked_text: BTreeMap<String, String>,
    /// Tools already reported as unavailable (once per activation).
    warned: BTreeSet<Tool>,
}

fn is_kotlin(path: &str) -> bool {
    path.ends_with(".kt") || path.ends_with(".kts")
}

/// The project containing `path`.
fn project_of(path: &str) -> Option<Project> {
    host::projects()
        .ok()?
        .into_iter()
        .filter(|p| p.root().is_ok_and(|r| is_under(path, &r)))
        .max_by_key(|p| p.root().map(|r| r.len()).unwrap_or(0))
}

/// Whether `path` is `root` or inside it (`/proj-x/a` is not inside `/proj`).
fn is_under(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || root.ends_with(['/', '\\']) || rest.starts_with(['/', '\\']))
}

fn enabled(tool: Tool) -> bool {
    host::setting_bool(tool.name()).unwrap_or(true)
}

/// The first line a failed run printed that says something.
fn failure(tool: Tool, status: Option<i32>, out: &openidea_plugin::ProcessOutput) -> Error {
    let line =
        out.stderr.lines().chain(out.stdout.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("no report");
    let status = status.map_or("killed".to_owned(), |s| format!("exit {s}"));
    Error::Failed(format!("{}: {line} ({status})", tool.name()))
}

fn detekt(project: &Project, path: &str) -> Result<Vec<Finding>, Error> {
    let report = format!("{}/detekt-report.sarif", fs::plugin_dir()?);
    let mut args = vec!["--input".to_owned(), path.to_owned(), "--report".into(), format!("sarif:{report}")];
    let config = host::setting_string("detektConfig")
        .filter(|c| !c.trim().is_empty())
        .or_else(|| DETEKT_CONFIGS.iter().find(|c| project.read_file(c).is_ok()).map(|c| (*c).to_owned()));
    if let Some(config) = config {
        args.extend(["--config".into(), config]);
        if host::setting_bool("buildUponDefaultConfig").unwrap_or(true) {
            args.push("--build-upon-default-config".into());
        }
    }
    if let Some(baseline) = DETEKT_BASELINES.iter().find(|b| project.read_file(b).is_ok()) {
        args.extend(["--baseline".into(), (*baseline).to_owned()]);
    }
    let out = project.run_process("detekt", &args, None)?;
    // 0: clean, 2: findings over `maxIssues`; 1 is an error, 3 an invalid configuration.
    // Only a successful run writes the report, so an older one is never read as this one.
    match out.status {
        Some(0 | 2) => {
            let sarif = fs::read_file(&report).map_err(|_| failure(Tool::Detekt, out.status, &out))?;
            sarif::parse(Tool::Detekt, &sarif).map_err(|e| Error::Failed(format!("detekt: {e}")))
        }
        status => Err(failure(Tool::Detekt, status, &out)),
    }
}

fn ktlint(project: &Project, path: &str) -> Result<Vec<Finding>, Error> {
    let args = vec!["--reporter=sarif".to_owned(), path.to_owned()];
    let out = project.run_process("ktlint", &args, None)?;
    // 0: clean, 1: findings (or an error, then without a report).
    match out.status {
        Some(0 | 1) if out.stdout.contains("\"runs\"") => {
            sarif::parse(Tool::Ktlint, &out.stdout).map_err(|e| Error::Failed(format!("ktlint: {e}")))
        }
        status => Err(failure(Tool::Ktlint, status, &out)),
    }
}

fn utf16(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// The word at UTF-16 column `col`, or the one character there: `(start, end)`.
fn word_at(line: &str, col: u32) -> (u32, u32) {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut pos = 0;
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if pos >= col {
            break;
        }
        pos += c.len_utf16() as u32;
        chars.next();
    }
    let mut end = pos;
    let mut first = true;
    for c in chars {
        if !first && !word(c) {
            break;
        }
        end += c.len_utf16() as u32;
        if first && !word(c) {
            break;
        }
        first = false;
    }
    (pos, end)
}

/// The finding's place in `text`, kept inside its lines.
fn range(f: &Finding, text: &str) -> Range {
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let len = |l: u32| lines.get(l as usize).map_or(0, |t| utf16(t));
    let line = f.line.min(lines.len().saturating_sub(1) as u32);
    let whole = Range::new(Position::new(line, 0), Position::new(line, len(line)));
    match (f.column, f.end) {
        (Some(c), _) if c >= len(line) => whole,
        (Some(c), Some((el, ec))) if (el, ec) > (line, c) && (el as usize) < lines.len() => {
            Range::new(Position::new(line, c), Position::new(el, ec.min(len(el))))
        }
        (Some(c), _) => {
            let (s, e) = word_at(lines[line as usize], c);
            Range::new(Position::new(line, s), Position::new(line, e))
        }
        (None, _) => whole,
    }
}

fn diagnostic(f: &Finding, text: &str) -> Diagnostic {
    let mut d = Diagnostic::new(range(f, text), f.severity, f.message.clone()).code(f.rule.clone());
    d.source = Some(f.tool.name().into());
    d
}

impl KotlinLint {
    fn clear(&mut self, path: &str) -> Result<(), Error> {
        self.findings.remove(path);
        self.checked_text.remove(path);
        host::publish_diagnostics(path, &[])
    }

    /// Runs the enabled tools on `path` and publishes what they found.
    /// Returns the number of findings and the tools' failures;
    /// `Err(Cancelled)` when a newer save (or the user) cancelled it.
    fn check(&mut self, path: &str) -> Result<(usize, Vec<String>), Error> {
        let project = project_of(path).ok_or_else(|| Error::NotFound(format!("{path} is not in an open project")))?;
        self.clear(path)?;
        let text = project.read_file(path)?;
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let task = host::begin_task(&format!("detekt + ktlint: {name}"), true);
        let mut findings = Vec::new();
        let mut failures = Vec::new();
        for tool in [Tool::Detekt, Tool::Ktlint].into_iter().filter(|t| enabled(*t)) {
            let result = match tool {
                Tool::Detekt => detekt(&project, path),
                Tool::Ktlint => ktlint(&project, path),
            };
            if task.is_cancelled() {
                return Err(Error::Cancelled);
            }
            match result {
                Ok(found) => findings.extend(found.into_iter().filter(|f| sarif::same_file(&f.file, path))),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                // Not installed or not granted: say so once, keep the other tool.
                Err(e @ (Error::Denied(_) | Error::NotFound(_))) => {
                    if self.warned.insert(tool) {
                        failures.push(format!(
                            "{} is unavailable ({e}). Install it (`brew install {}`) and grant process:{} in Settings › Plugins, or turn it off in this plugin's settings.",
                            tool.name(),
                            tool.name(),
                            tool.name()
                        ));
                    }
                }
                Err(e) => failures.push(e.to_string()),
            }
        }
        // A filesystem batch queued during this run will check the newer contents.
        if project.read_file(path)? != text {
            return Err(Error::Stale);
        }
        let diagnostics: Vec<Diagnostic> = findings.iter().map(|f| diagnostic(f, &text)).collect();
        host::publish_diagnostics(path, &diagnostics)?;
        let n = findings.len();
        self.findings.insert(path.to_owned(), findings);
        self.checked_text.insert(path.to_owned(), text);
        Ok((n, failures))
    }
}

fn action(title: String, path: &str, s: Suppression) -> Option<CodeAction> {
    let Suppression::Edit(edit) = s else { return None };
    Some(CodeAction {
        title,
        kind: Some("quickfix".into()),
        preferred: false,
        edits: vec![(path.to_owned(), vec![edit])],
        data: None,
    })
}

impl Providers for KotlinLint {
    fn code_actions(&mut self, doc: &Document, selection: Range) -> Vec<CodeAction> {
        if self.checked_text.get(&doc.path) != Some(&doc.text) {
            return Vec::new();
        }
        let Some(findings) = self.findings.get(&doc.path) else { return Vec::new() };
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for f in findings.iter().filter(|f| f.line >= selection.start.line && f.line <= selection.end.line) {
            let id = f.suppress_id();
            let tool = f.tool.name();
            if let Some(decl) = suppress::enclosing(&doc.text, f.line) {
                let target = if decl.name.is_empty() { decl.keyword.to_owned() } else { format!("'{}'", decl.name) };
                let title = format!("{tool}: Suppress {} for {target}", f.rule);
                if seen.insert(title.clone())
                    && let Some(a) = action(title, &doc.path, suppress::for_declaration(&doc.text, &decl, &id))
                {
                    out.push(a);
                }
            }
            let title = format!("{tool}: Suppress {} in file", f.rule);
            if seen.insert(title.clone())
                && let Some(a) = action(title, &doc.path, suppress::for_file(&doc.text, &id))
            {
                out.push(a);
            }
        }
        out
    }
}

impl Plugin for KotlinLint {
    fn activate(&mut self, _ctx: &openidea_plugin::Context) -> Result<(), Error> {
        for kind in ["file-saved", "files-changed"] {
            host::subscribe_paths(kind, &["**/*.kt", "**/*.kts"])?;
        }
        Ok(())
    }

    fn run_command(&mut self, id: &str, ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        let editor = ctx.editor().ok_or("no file is open")?;
        let path = editor.path();
        match id {
            "check" => {
                if !is_kotlin(&path) {
                    return Err(Error::Invalid(format!("{path} is not a Kotlin file")));
                }
                let (n, failures) = self.check(&path)?;
                if !failures.is_empty() {
                    return Err(Error::Failed(failures.join("\n")));
                }
                Ok(Some(format!("{n} problems")))
            }
            "clear" => {
                self.clear(&path)?;
                Ok(None)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], _ctx: &Invocation) {
        let mut changed = BTreeMap::new();
        for event in events {
            match event {
                Event::FileSaved(path) if is_kotlin(path) => {
                    changed.insert(path.as_str(), true);
                }
                Event::FilesChanged(batch) => {
                    for path in &batch.removed {
                        if is_kotlin(path) {
                            let _ = self.clear(path);
                        }
                    }
                    for path in batch.created.iter().chain(&batch.modified).filter(|p| is_kotlin(p)) {
                        changed.entry(path.as_str()).or_insert(false);
                    }
                }
                _ => {}
            }
        }
        for (path, saved) in changed {
            // A save and the watcher can report the same write in separate batches.
            if !saved
                && self
                    .checked_text
                    .get(path)
                    .is_some_and(|text| project_of(path).and_then(|p| p.read_file(path).ok()).as_ref() == Some(text))
            {
                continue;
            }
            match self.check(path) {
                Ok((_, failures)) => {
                    for f in failures {
                        host::notify(NotificationLevel::Warning, "detekt + ktlint", &f);
                    }
                }
                Err(Error::Stale) => continue, // the watcher will deliver the newer contents
                Err(Error::Cancelled) => return, // a newer change takes over
                Err(e) => {
                    host::notify(NotificationLevel::Warning, "detekt + ktlint", &e.to_string());
                    return;
                }
            }
        }
    }
}

register_plugin!(KotlinLint, providers);

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use openidea_plugin::ProcessOutput;
    use openidea_plugin::mock::MockHost;

    const ALL: [&str; 5] = ["project.read", "events", "storage", "process:detekt", "process:ktlint"];
    const TEXT: &str = "package demo\n\nimport a.*\n\nfun total() = 42\n";
    const DETEKT: &str = r#"{"version":"2.1.0","runs":[{"results":[
      {"ruleId":"detekt.style.MagicNumber","level":"warning","message":{"text":"This expression contains a magic number."},
       "locations":[{"physicalLocation":{"artifactLocation":{"uri":"file:///project/src/A.kt"},
         "region":{"startLine":5,"startColumn":15,"endLine":5,"endColumn":17}}}]},
      {"ruleId":"detekt.style.MagicNumber","level":"warning","message":{"text":"Elsewhere."},
       "locations":[{"physicalLocation":{"artifactLocation":{"uri":"file:///project/src/B.kt"},
         "region":{"startLine":1,"startColumn":1}}}]}]}]}"#;
    const KTLINT: &str = r#"{"version":"2.1.0","runs":[{"originalUriBaseIds":{"%SRCROOT%":{"uri":"file:///project/"}},"results":[
      {"ruleId":"standard:no-wildcard-imports","level":"error","message":{"text":"Wildcard import"},
       "locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/A.kt","uriBaseId":"%SRCROOT%"},
         "region":{"startLine":3,"startColumn":1}}}]}]}]}"#;

    fn report_path() -> String {
        format!("{}/detekt-report.sarif", fs::plugin_dir().unwrap())
    }

    fn host(caps: &[&str]) -> MockHost {
        let host = MockHost::install().with_capabilities(caps);
        host.add_file("src/A.kt", TEXT);
        host.add_external_file(&report_path(), DETEKT);
        host.on_process("detekt", |_, _| {
            Ok(ProcessOutput { status: Some(2), stdout: String::new(), stderr: String::new() })
        });
        host.on_process("ktlint", |_, _| {
            Ok(ProcessOutput { status: Some(1), stdout: KTLINT.into(), stderr: String::new() })
        });
        host
    }

    fn doc() -> Document {
        Document { path: "/project/src/A.kt".into(), language: "kotlin".into(), version: 1, text: TEXT.into() }
    }

    fn line(n: u32) -> Range {
        Range::new(Position::new(n, 0), Position::new(n, 0))
    }

    #[test]
    fn checks_saved_kotlin_files_with_both_tools() {
        let host = host(&ALL);
        let mut p = KotlinLint::default();
        p.handle_events(
            &[Event::FileSaved("/project/src/A.kt".into()), Event::FileSaved("/project/README.md".into())],
            &host.invocation(),
        );
        let runs = host.process_runs();
        assert_eq!(runs.iter().map(|r| r.program.as_str()).collect::<Vec<_>>(), ["detekt", "ktlint"]);
        assert_eq!(runs[0].args, ["--input", "/project/src/A.kt", "--report", &format!("sarif:{}", report_path())]);
        assert_eq!(runs[1].args, ["--reporter=sarif", "/project/src/A.kt"]);
        assert_eq!(runs[0].cwd.as_deref(), Some("/project"));

        let d = &host.diagnostics()["/project/src/A.kt"];
        assert_eq!(d.len(), 2, "B.kt's finding is not A.kt's");
        assert_eq!((d[0].code.as_deref(), d[0].source.as_deref()), (Some("MagicNumber"), Some("detekt")));
        assert_eq!(d[0].range, Range::new(Position::new(4, 14), Position::new(4, 16)));
        assert_eq!(
            (d[1].code.as_deref(), d[1].source.as_deref()),
            (Some("standard:no-wildcard-imports"), Some("ktlint"))
        );
        assert_eq!(d[1].range, Range::new(Position::new(2, 0), Position::new(2, 6)), "the word at the column");
        assert!(host.tasks()[0].ended);
        assert!(host.notifications().is_empty());
    }

    #[test]
    fn quick_fixes_suppress_the_rule() {
        let host = host(&ALL);
        let mut p = KotlinLint::default();
        p.handle_events(&[Event::FileSaved("/project/src/A.kt".into())], &host.invocation());

        let actions = p.code_actions(&doc(), line(4));
        let titles: Vec<_> = actions.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["detekt: Suppress MagicNumber for 'total'", "detekt: Suppress MagicNumber in file"]);
        assert_eq!(actions[0].edits[0].1[0].text, "@Suppress(\"MagicNumber\")\n");
        assert_eq!(actions[0].edits[0].1[0].range, line(4));
        assert_eq!(actions[1].edits[0].1[0].text, "@file:Suppress(\"MagicNumber\")\n\n");
        assert_eq!(actions[1].edits[0].1[0].range, line(0));

        let actions = p.code_actions(&doc(), line(2));
        let titles: Vec<_> = actions.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["ktlint: Suppress standard:no-wildcard-imports in file"], "an import has no declaration");
        assert_eq!(actions[0].edits[0].1[0].text, "@file:Suppress(\"ktlint:standard:no-wildcard-imports\")\n\n");
        assert!(p.code_actions(&doc(), line(1)).is_empty());
    }

    fn disk_change(path: &str) -> Event {
        Event::FilesChanged(openidea_plugin::FilesChanged { modified: vec![path.into()], ..Default::default() })
    }

    #[test]
    fn quick_fix_disk_write_refreshes_positions_and_undo_restores_findings() {
        let host = host(&ALL);
        let mut p = KotlinLint::default();
        let mut doc = doc();
        p.handle_events(&[Event::FileSaved(doc.path.clone())], &host.invocation());
        let fix = p.code_actions(&doc, line(2)).remove(0);
        let edit = &fix.edits[0].1[0];
        let start = openidea_plugin::text::offset(&doc.text, edit.range.start).unwrap();
        let end = openidea_plugin::text::offset(&doc.text, edit.range.end).unwrap();
        doc.text.replace_range(start..end, &edit.text);
        host.add_file("src/A.kt", &doc.text);
        assert!(p.code_actions(&doc, line(4)).is_empty(), "old findings must not target new text");

        // ktlint's wildcard warning is suppressed; detekt's number moves down two lines.
        host.on_process("ktlint", |_, _| {
            Ok(ProcessOutput { status: Some(0), stdout: r#"{"runs":[]}"#.into(), ..Default::default() })
        });
        host.add_external_file(
            &report_path(),
            &DETEKT.replace("\"startLine\":5", "\"startLine\":7").replace("\"endLine\":5", "\"endLine\":7"),
        );
        p.handle_events(&[disk_change(&doc.path)], &host.invocation());
        let d = &host.diagnostics()[&doc.path];
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range.start.line, 6);
        assert_eq!(p.code_actions(&doc, line(6)).len(), 2);

        // Undo is another disk write, with no explicit save event.
        host.add_file("src/A.kt", TEXT);
        host.add_external_file(&report_path(), DETEKT);
        host.on_process("ktlint", |_, _| {
            Ok(ProcessOutput { status: Some(1), stdout: KTLINT.into(), ..Default::default() })
        });
        p.handle_events(&[disk_change(&doc.path)], &host.invocation());
        assert_eq!(host.diagnostics()[&doc.path].len(), 2);
        assert_eq!(host.diagnostics()[&doc.path][0].range.start.line, 4);
    }

    #[test]
    fn disk_events_deduplicate_paths_and_saved_contents_and_clear_removed_files() {
        for path in ["/project/src/A.kt", "/project/src/A.kts"] {
            let host = host(&ALL);
            host.add_file(path, TEXT);
            let mut p = KotlinLint::default();
            p.handle_events(
                &[Event::FileSaved(path.into()), disk_change(path), disk_change("/project/README.md")],
                &host.invocation(),
            );
            assert_eq!(host.process_runs().len(), 2);
            p.handle_events(&[disk_change(path)], &host.invocation());
            assert_eq!(host.process_runs().len(), 2, "watcher duplicate of a save");
            p.handle_events(
                &[Event::FilesChanged(openidea_plugin::FilesChanged {
                    removed: vec![path.into()],
                    ..Default::default()
                })],
                &host.invocation(),
            );
            assert!(host.diagnostics().get(path).is_none_or(Vec::is_empty));
            assert!(!p.findings.contains_key(path));
            p.handle_events(
                &[Event::FilesChanged(openidea_plugin::FilesChanged {
                    created: vec![path.into(), path.into()],
                    ..Default::default()
                })],
                &host.invocation(),
            );
            assert_eq!(host.process_runs().len(), 4, "recreated files are checked again");
        }
    }

    #[test]
    fn the_projects_detekt_configuration_and_baseline_are_used() {
        let host = host(&ALL);
        host.add_file("config/detekt/detekt.yml", "style:\n  MagicNumber:\n    active: true\n");
        host.add_file("config/detekt/baseline.xml", "<SmellBaseline/>");
        let mut p = KotlinLint::default();
        p.handle_events(&[Event::FileSaved("/project/src/A.kt".into())], &host.invocation());
        assert_eq!(
            host.process_runs()[0].args[4..],
            [
                "--config",
                "config/detekt/detekt.yml",
                "--build-upon-default-config",
                "--baseline",
                "config/detekt/baseline.xml"
            ]
        );
        host.set_setting("detektConfig", "\"lint/detekt.yml\"");
        host.set_setting("buildUponDefaultConfig", "false");
        p.handle_events(&[Event::FileSaved("/project/src/A.kt".into())], &host.invocation());
        assert_eq!(host.process_runs()[2].args[4..6], ["--config", "lint/detekt.yml"]);
        assert!(!host.process_runs()[2].args.contains(&"--build-upon-default-config".to_owned()));
    }

    #[test]
    fn a_missing_tool_is_reported_once_and_the_other_still_runs() {
        let host = host(&["project.read", "events", "storage", "process:ktlint"]);
        let mut p = KotlinLint::default();
        for _ in 0..2 {
            p.handle_events(&[Event::FileSaved("/project/src/A.kt".into())], &host.invocation());
        }
        assert_eq!(host.notifications().len(), 1);
        assert!(host.notifications()[0].body.contains("process:detekt"));
        assert_eq!(host.diagnostics()["/project/src/A.kt"].len(), 1, "ktlint's");
    }

    #[test]
    fn tool_errors_are_reported_and_settings_turn_tools_off() {
        let host = host(&ALL);
        host.on_process("detekt", |_, _| {
            Ok(ProcessOutput { status: Some(3), stdout: String::new(), stderr: "Invalid config: foo\nat …".into() })
        });
        host.set_setting("ktlint", "false");
        let mut p = KotlinLint::default();
        p.handle_events(&[Event::FileSaved("/project/src/A.kt".into())], &host.invocation());
        assert_eq!(host.process_runs().len(), 1, "ktlint is off");
        assert_eq!(host.notifications()[0].body, "detekt: Invalid config: foo (exit 3)");
        assert!(host.diagnostics().get("/project/src/A.kt").is_none_or(|d| d.is_empty()));
    }

    /// The real reports of detekt 1.23.8 and ktlint 1.8.0, end to end.
    #[test]
    fn real_reports_become_diagnostics_and_fixes() {
        let text = include_str!("../tests/fixtures/Shop.kt");
        let host = MockHost::install().with_capabilities(&ALL);
        host.add_file("src/Shop.kt", text);
        host.add_external_file(&report_path(), include_str!("../tests/fixtures/detekt-1.23.8.sarif"));
        host.on_process("detekt", |_, _| Ok(ProcessOutput { status: Some(2), ..Default::default() }));
        host.on_process("ktlint", |_, _| {
            let stdout = include_str!("../tests/fixtures/ktlint-1.8.0.sarif").to_owned();
            Ok(ProcessOutput { status: Some(1), stdout, stderr: String::new() })
        });
        let mut p = KotlinLint::default();
        p.handle_events(&[Event::FileSaved("/project/src/Shop.kt".into())], &host.invocation());
        assert!(host.notifications().is_empty(), "{:?}", host.notifications());
        let d = &host.diagnostics()["/project/src/Shop.kt"];
        assert_eq!(d.iter().filter(|d| d.source.as_deref() == Some("detekt")).count(), 4);
        assert_eq!(d.iter().filter(|d| d.source.as_deref() == Some("ktlint")).count(), 5);
        let magic = d.iter().find(|d| d.code.as_deref() == Some("MagicNumber")).unwrap();
        assert_eq!(magic.range, Range::new(Position::new(7, 17), Position::new(7, 19)), "`42`");

        let doc =
            Document { path: "/project/src/Shop.kt".into(), language: "kotlin".into(), version: 1, text: text.into() };
        let titles: Vec<String> = p.code_actions(&doc, line(7)).into_iter().map(|a| a.title).collect();
        assert_eq!(titles, ["detekt: Suppress MagicNumber for 'list'", "detekt: Suppress MagicNumber in file"]);
        let titles: Vec<String> = p.code_actions(&doc, line(5)).into_iter().map(|a| a.title).collect();
        assert!(titles.contains(&"detekt: Suppress FunctionNaming for 'Total'".to_owned()), "{titles:?}");
        assert!(titles.contains(&"ktlint: Suppress standard:function-naming for 'Total'".to_owned()), "{titles:?}");
    }

    #[test]
    fn ranges_stay_in_the_text() {
        let f = |line, column, end| Finding {
            tool: Tool::Ktlint,
            file: String::new(),
            line,
            column,
            end,
            severity: openidea_plugin::Severity::Warning,
            message: String::new(),
            rule: String::new(),
        };
        let text = "val a = 1\nval bé = 2";
        assert_eq!(range(&f(0, None, None), text), Range::new(Position::new(0, 0), Position::new(0, 9)));
        assert_eq!(range(&f(1, Some(4), None), text), Range::new(Position::new(1, 4), Position::new(1, 6)));
        assert_eq!(
            range(&f(1, Some(7), None), text),
            Range::new(Position::new(1, 7), Position::new(1, 8)),
            "one symbol"
        );
        assert_eq!(range(&f(0, Some(30), None), text), Range::new(Position::new(0, 0), Position::new(0, 9)));
        assert_eq!(range(&f(9, None, None), text).start.line, 1);
        assert_eq!(range(&f(0, Some(0), Some((1, 99))), text), Range::new(Position::new(0, 0), Position::new(1, 10)));
    }
}
