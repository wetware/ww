//! High-signal review lint for explicit Glia child grants.
//!
//! This is review tooling, not a security boundary. Runtime confinement still
//! comes from constructive child creation and the immutable authority record.

use glia::Val;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const CAPABILITY_CATALOG_JSON: &str =
    include_str!("../../../doc/generated/capability-catalog.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub rule: &'static str,
    pub severity: Severity,
    pub path: PathBuf,
    pub line: usize,
    pub found: String,
    pub risk: String,
    pub fix: String,
    pub suppression: String,
}

struct LintContext<'a> {
    source: &'a str,
    next_search_offsets: BTreeMap<(String, String), usize>,
    used_suppressions: BTreeSet<(usize, String, String)>,
}

impl<'a> LintContext<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            next_search_offsets: BTreeMap::new(),
            used_suppressions: BTreeSet::new(),
        }
    }

    fn line_of(&mut self, rule: &str, needle: &str) -> usize {
        self.line_of_occurrences(rule, needle, 1)
    }

    fn line_of_occurrences(&mut self, rule: &str, needle: &str, occurrences: usize) -> usize {
        let key = (rule.to_owned(), needle.to_owned());
        let mut search_from = self.next_search_offsets.get(&key).copied().unwrap_or(0);
        let mut first_offset = None;
        for _ in 0..occurrences.max(1) {
            let Some(relative_offset) = self.source[search_from..].find(needle) else {
                break;
            };
            let offset = search_from + relative_offset;
            first_offset.get_or_insert(offset);
            search_from = offset + needle.len();
        }
        let Some(first_offset) = first_offset else {
            return 1;
        };
        self.next_search_offsets.insert(key, search_from);
        self.source[..first_offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1
    }

    fn is_suppressed(
        &mut self,
        diagnostic_line: usize,
        rule: &str,
        grant_name: Option<&str>,
    ) -> bool {
        let lines = self.source.lines().collect::<Vec<_>>();
        let start = diagnostic_line.saturating_sub(4);
        let end = diagnostic_line.saturating_sub(1).min(lines.len());
        let matched_line = (start..end).rev().find(|line_index| {
            let suppression = (
                *line_index,
                rule.to_owned(),
                grant_name.unwrap_or_default().to_owned(),
            );
            !self.used_suppressions.contains(&suppression)
                && suppression_matches(lines[*line_index], rule, grant_name)
        });
        if let Some(line_index) = matched_line {
            self.used_suppressions.insert((
                line_index,
                rule.to_owned(),
                grant_name.unwrap_or_default().to_owned(),
            ));
            true
        } else {
            false
        }
    }
}

pub fn lint_path(path: &Path) -> Result<Vec<Diagnostic>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(lint_source(path, &source))
}

pub fn lint_source(path: &Path, source: &str) -> Vec<Diagnostic> {
    let forms = match glia::read_many(source) {
        Ok(forms) => forms,
        Err(error) => {
            return vec![Diagnostic {
                rule: "GLIA001",
                severity: Severity::Error,
                path: path.to_owned(),
                line: 1,
                found: format!("Glia source cannot be parsed or structurally validated: {error}"),
                risk: "Malformed or duplicate grant syntax cannot be reviewed and will fail before spawn."
                    .to_owned(),
                fix: "Fix the reader error; use (cell image :grants {:name capability}) with unique keyword keys."
                    .to_owned(),
                suppression: "Structural errors cannot be suppressed.".to_owned(),
            }]
        }
    };

    let mut diagnostics = Vec::new();
    let mut context = LintContext::new(source);
    for form in &forms {
        walk(form, false, path, &mut context, &mut diagnostics);
    }
    lint_broad_host_alternative(path, &mut context, &mut diagnostics);
    diagnostics
}

pub fn collect_glia_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in paths {
        collect_one(path, &mut files)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_one(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.is_file() {
        if path.extension().and_then(|value| value.to_str()) == Some("glia") {
            files.push(path.to_owned());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!("lint path does not exist: {}", path.display()));
    }
    let entries =
        std::fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", path.display()))?;
        let child = entry.path();
        if child
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name == "target" || name.starts_with('.'))
        {
            continue;
        }
        collect_one(&child, files)?;
    }
    Ok(())
}

fn walk(
    value: &Val,
    inside_with: bool,
    path: &Path,
    context: &mut LintContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match value {
        Val::List(items) => {
            let head = items.first().and_then(symbol);
            if head == Some("quote") {
                return;
            }
            let is_with = head == Some("with");
            if head == Some("cell") {
                lint_cell(items, inside_with, path, context, diagnostics);
            }
            if is_spawn_call(items) {
                lint_spawn_strings(value, path, context, diagnostics);
            }
            for item in items {
                walk(item, inside_with || is_with, path, context, diagnostics);
            }
        }
        Val::Vector(items) | Val::Set(items) => {
            for item in items {
                walk(item, inside_with, path, context, diagnostics);
            }
        }
        Val::Map(map) => {
            for (key, child) in map.iter() {
                walk(key, inside_with, path, context, diagnostics);
                walk(child, inside_with, path, context, diagnostics);
            }
        }
        _ => {}
    }
}

fn lint_cell(
    items: &[Val],
    inside_with: bool,
    path: &Path,
    context: &mut LintContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let grants = items
        .windows(2)
        .find(|window| keyword(&window[0]) == Some("grants"))
        .map(|window| &window[1]);
    if grants.is_none() && inside_with {
        let line = context.line_of("WWG103", "(cell");
        push_if_unsuppressed(
            Diagnostic {
                rule: "WWG103",
                severity: Severity::Warning,
                path: path.to_owned(),
                line,
                found: "A cell without :grants appears under `with`; lexical capability capture no longer grants those bindings.".to_owned(),
                risk: "The child starts with zero application capabilities even when a nearby binding looks grant-like.".to_owned(),
                fix: "Rewrite as (cell image :grants {:expected-name capability-binding}), or move a deliberate zero-grant cell out of the misleading `with` scope.".to_owned(),
                suppression: suppression_help("WWG103", None),
            },
            context,
            None,
            diagnostics,
        );
    }
    let Some(grants) = grants else {
        return;
    };
    match grants {
        Val::Map(map) => {
            for (key, _) in map.iter() {
                let Some(name) = keyword(key) else {
                    continue;
                };
                if is_sensitive_grant(name) {
                    let line = context.line_of("WWG101", &format!(":{name}"));
                    push_if_unsuppressed(
                        Diagnostic {
                            rule: "WWG101",
                            severity: Severity::Warning,
                            path: path.to_owned(),
                            line,
                            found: format!(
                                "Sensitive capability `{name}` is explicitly granted without an adjacent justification marker."
                            ),
                            risk: sensitive_risk(name).to_owned(),
                            fix: sensitive_fix(name).to_owned(),
                            suppression: suppression_help("WWG101", Some(name)),
                        },
                        context,
                        Some(name),
                        diagnostics,
                    );
                }
            }
        }
        Val::Sym(_) => {}
        _ => {
            let line = context.line_of("WWG201", ":grants");
            push_if_unsuppressed(
                Diagnostic {
                rule: "WWG201",
                severity: Severity::Advisory,
                path: path.to_owned(),
                line,
                found: "The cell grant map is computed inline through a non-literal expression."
                    .to_owned(),
                risk: "Reviewers cannot determine the child's complete birth authority from a literal map or a clearly named bundle."
                    .to_owned(),
                fix: "Bind the reviewed map to a descriptive symbol, then write (cell image :grants reviewed-grants)."
                    .to_owned(),
                suppression: suppression_help("WWG201", None),
                },
                context,
                None,
                diagnostics,
            )
        }
    }
}

fn lint_spawn_strings(
    value: &Val,
    path: &Path,
    context: &mut LintContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let rendered = value.to_string().to_ascii_lowercase();
    let credential = [
        "bearer",
        "token",
        "password",
        "secret",
        "api-key",
        "api_key",
        "credential",
    ]
    .into_iter()
    .find(|needle| rendered.contains(needle));
    let Some(credential) = credential else {
        return;
    };
    if !rendered.contains(":args") && !rendered.contains(":env") {
        return;
    }
    let line =
        context.line_of_occurrences("WWG102", credential, rendered.matches(credential).count());
    push_if_unsuppressed(
        Diagnostic {
            rule: "WWG102",
            severity: Severity::Warning,
            path: path.to_owned(),
            line,
            found: format!(
                "Credential-like string `{credential}` is passed through :args or :env near an Executor spawn."
            ),
            risk: "Args and env are substrate bearer-string channels; they are copyable, loggable, and not constrained by the child's capability record.".to_owned(),
            fix: "Grant a scoped capability that performs the operation, or grant an explicit secret-provider capability instead of passing the bearer value.".to_owned(),
            suppression: suppression_help("WWG102", None),
        },
        context,
        None,
        diagnostics,
    );
}

fn lint_broad_host_alternative(
    path: &Path,
    context: &mut LintContext<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if context.source.contains("(attenuate host")
        && (context.source.contains(":host host}") || context.source.contains(":host host "))
    {
        let line = context.line_of("WWG104", ":host host");
        push_if_unsuppressed(
            Diagnostic {
                rule: "WWG104",
                severity: Severity::Warning,
                path: path.to_owned(),
                line,
                found: "A broad `host` reference is granted while an attenuated Host value is visibly constructed in the same file.".to_owned(),
                risk: "Host can expose the node network bundle; granting the broader reference defeats the visible least-authority alternative.".to_owned(),
                fix: "Grant the attenuated binding under the conventional key, for example :grants {:host status-host}.".to_owned(),
                suppression: suppression_help("WWG104", None),
            },
            context,
            None,
            diagnostics,
        );
    }
}

fn push_if_unsuppressed(
    diagnostic: Diagnostic,
    context: &mut LintContext<'_>,
    grant_name: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !context.is_suppressed(diagnostic.line, diagnostic.rule, grant_name) {
        diagnostics.push(diagnostic);
    }
}

fn suppression_matches(line: &str, rule: &str, grant_name: Option<&str>) -> bool {
    let marker = format!("grant-lint: allow {rule}");
    let Some(position) = line.find(&marker) else {
        return false;
    };
    let remainder = line[position + marker.len()..].trim();
    let Some((scope, reason)) = remainder.split_once("--") else {
        return false;
    };
    let scope = scope.trim();
    let scope_matches = match grant_name {
        Some(name) => scope == name,
        None => scope.is_empty(),
    };
    scope_matches && !reason.trim().is_empty()
}

fn suppression_help(rule: &str, grant_name: Option<&str>) -> String {
    match grant_name {
        Some(name) => format!(
            "For an intentional case, add immediately above: ;; grant-lint: allow {rule} {name} -- <specific reason>"
        ),
        None => format!(
            "For an intentional case, add immediately above: ;; grant-lint: allow {rule} -- <specific reason>"
        ),
    }
}

fn symbol(value: &Val) -> Option<&str> {
    match value {
        Val::Sym(value) => Some(value),
        _ => None,
    }
}

fn keyword(value: &Val) -> Option<&str> {
    match value {
        Val::Keyword(value) => Some(value),
        _ => None,
    }
}

fn is_spawn_call(items: &[Val]) -> bool {
    symbol(items.first().unwrap_or(&Val::Nil)) == Some("perform")
        && items.iter().any(|value| keyword(value) == Some("spawn"))
}

fn is_sensitive_grant(name: &str) -> bool {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            let catalog: serde_json::Value =
                serde_json::from_str(CAPABILITY_CATALOG_JSON).expect("checked catalog JSON");
            catalog["capabilities"]
                .as_array()
                .expect("catalog capabilities")
                .iter()
                .filter(|entry| {
                    entry["sensitivity"].as_str() == Some("critical")
                        || matches!(
                            entry["effectClass"].as_str(),
                            Some("outbound-network" | "content-backend-read")
                        )
                })
                .filter_map(|entry| entry["conventionalName"].as_str().map(str::to_owned))
                .collect()
        })
        .contains(name)
}

fn sensitive_risk(name: &str) -> &'static str {
    match name {
        "membrane" => "Membrane grafts the trusted root's configured host authority and is not intended for ordinary children.",
        "runtime" => "Runtime selects arbitrary WASM images; a preloaded image-bound Executor is usually narrower.",
        "identity" => "Identity can issue domain signers backed by the node identity.",
        "routing" => "Routing includes network announcements, content transforms, resolution, and IPNS publishing.",
        "http-client" => "HttpClient performs outbound requests and can transmit data to configured domains.",
        "ipfs" => "The Ipfs RPC reference exposes backend reads beyond ordinary image-rooted WASI access.",
        "authority" => "Authority constructs authenticated session gates over supplied capabilities.",
        "vat-listener" => "VatListener can publish capabilities, including through the explicit raw escape hatch.",
        _ => "This capability is classified as sensitive; consult its catalog entry and security notes before granting it.",
    }
}

fn sensitive_fix(name: &str) -> &'static str {
    match name {
        "membrane" => "Do not grant Membrane to ordinary children; delegate only the specific returned references they need.",
        "runtime" => "Load the intended image in the parent and grant {:executor image-executor}; retain Runtime only for a trusted load-any child.",
        "identity" => "Select the domain in the parent and grant {:signer scoped-signer}.",
        "routing" => "Attenuate to the needed methods, for example {:routing (attenuate routing [:hash])}.",
        "http-client" => "Use a domain-scoped --http-dial configuration and grant only the resulting http-client reference.",
        "ipfs" => "Use the child's fixed image/known-CID filesystem substrate, or document why the backend RPC reference is required.",
        "authority" => "Construct the Terminal in trusted configuration and grant {:terminal service-terminal}.",
        "vat-listener" => "Publish from trusted configuration or grant a service-specific publisher wrapper.",
        _ => "Consult the capability catalog entry and grant its documented narrower reference or attenuation, if available.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(source: &str) -> Vec<Diagnostic> {
        lint_source(Path::new("fixture.glia"), source)
    }

    fn rules(source: &str) -> Vec<&'static str> {
        diagnostics(source)
            .iter()
            .map(|diagnostic| diagnostic.rule)
            .collect()
    }

    fn rule_lines(source: &str, rule: &str) -> Vec<usize> {
        diagnostics(source)
            .iter()
            .filter(|diagnostic| diagnostic.rule == rule)
            .map(|diagnostic| diagnostic.line)
            .collect()
    }

    #[test]
    fn catches_sensitive_grant_and_reasoned_suppression_is_narrow() {
        assert_eq!(
            rules("(cell image :grants {:runtime runtime})"),
            vec!["WWG101"]
        );
        assert!(rules(
            ";; grant-lint: allow WWG101 runtime -- trusted compiler service\n(cell image :grants {:runtime runtime})"
        )
        .is_empty());
        assert_eq!(
            rules(
                ";; grant-lint: allow WWG101 runtime --\n(cell image :grants {:runtime runtime})"
            ),
            vec!["WWG101"]
        );
    }

    #[test]
    fn catches_bearer_strings_stale_with_and_broad_host() {
        assert_eq!(
            rules("(perform executor :spawn :env {:api-token secret-token})"),
            vec!["WWG102"]
        );
        assert_eq!(
            rules("(with [status-host host] (cell image))"),
            vec!["WWG103"]
        );
        assert_eq!(
            rules(
                "(def status-host (attenuate host [:id]))\n;; grant-lint: allow WWG101 host -- trusted status test\n(cell image :grants {:host host})"
            ),
            vec!["WWG104"]
        );
    }

    #[test]
    fn first_sensitive_suppression_does_not_hide_the_second_occurrence() {
        let source = ";; grant-lint: allow WWG101 runtime -- trusted first loader\n\
                      (cell first :grants {:runtime runtime})\n\
                      (cell second :grants {:runtime runtime})";
        assert_eq!(rule_lines(source, "WWG101"), vec![3]);
    }

    #[test]
    fn second_sensitive_suppression_only_hides_the_second_occurrence() {
        let source = "(cell first :grants {:runtime runtime})\n\
                      ;; grant-lint: allow WWG101 runtime -- trusted second loader\n\
                      (cell second :grants {:runtime runtime})";
        assert_eq!(rule_lines(source, "WWG101"), vec![1]);
    }

    #[test]
    fn identical_sensitive_grants_report_distinct_source_lines() {
        let source = "(cell first :grants {:runtime runtime})\n\n\
                      (cell second :grants {:runtime runtime})";
        assert_eq!(rule_lines(source, "WWG101"), vec![1, 3]);
    }

    #[test]
    fn repeated_wwg102_and_wwg103_sites_have_independent_suppression_and_lines() {
        let credential_source = ";; grant-lint: allow WWG102 -- trusted first launch\n\
                                 (perform executor :spawn :env {:api-token secret-token})\n\
                                 (perform executor :spawn :env {:api-token secret-token})";
        assert_eq!(rule_lines(credential_source, "WWG102"), vec![3]);

        let stale_with_source =
            ";; grant-lint: allow WWG103 -- deliberate first zero-grant child\n\
                                 (with [first value] (cell first))\n\
                                 (with [second value] (cell second))";
        assert_eq!(rule_lines(stale_with_source, "WWG103"), vec![3]);

        let second_suppressions = "(perform executor :spawn :env {:api-token secret-token})\n\
                                   ;; grant-lint: allow WWG102 -- trusted second launch\n\
                                   (perform executor :spawn :env {:api-token secret-token})\n\
                                   (with [first value] (cell first))\n\
                                   ;; grant-lint: allow WWG103 -- deliberate second zero-grant child\n\
                                   (with [second value] (cell second))";
        assert_eq!(rule_lines(second_suppressions, "WWG102"), vec![1]);
        assert_eq!(rule_lines(second_suppressions, "WWG103"), vec![4]);
    }

    #[test]
    fn membrane_grant_uses_membrane_specific_guidance() {
        let diagnostics = diagnostics("(cell image :grants {:membrane membrane})");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].rule, "WWG101");
        assert!(diagnostics[0].risk.contains("trusted root"));
        assert!(diagnostics[0].fix.contains("Do not grant Membrane"));
        assert!(!diagnostics[0].risk.contains("Host exposes"));
        assert!(sensitive_risk("future-sensitive-capability").contains("catalog"));
        assert!(sensitive_fix("future-sensitive-capability").contains("catalog"));
    }

    #[test]
    fn catches_obscure_computed_map_but_allows_literal_named_and_zero_grants() {
        assert_eq!(
            rules("(cell image :grants (merge base extra))"),
            vec!["WWG201"]
        );
        assert!(rules("(cell image)").is_empty());
        assert!(rules("(cell image :grants {})").is_empty());
        assert!(rules("(cell image :grants reviewed-bundle)").is_empty());
        assert!(rules("(cell image :grants {:executor worker-executor})").is_empty());
    }

    #[test]
    fn structural_reader_errors_are_not_suppressible() {
        let diagnostics = lint_source(
            Path::new("fixture.glia"),
            ";; grant-lint: allow GLIA001 -- no\n(cell image :grants {:db a :db b})",
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(diagnostics[0].rule, "GLIA001");
    }

    #[test]
    fn canonical_grant_examples_parse_and_lint_cleanly() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let examples = root.join("examples/grants");
        let files = collect_glia_files(&[examples]).expect("canonical example files");
        assert_eq!(files.len(), 5);
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read canonical example");
            glia::read_many(&source)
                .unwrap_or_else(|error| panic!("{} does not parse: {error}", file.display()));
            let diagnostics = lint_source(&file, &source);
            assert!(
                diagnostics.is_empty(),
                "{} produced {diagnostics:#?}",
                file.display()
            );
        }
    }
}
