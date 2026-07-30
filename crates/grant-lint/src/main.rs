use grant_lint::{collect_glia_files, lint_path, Severity};
use std::path::PathBuf;

fn main() {
    match run() {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) => {
            eprintln!("grant-lint: {error}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<i32, String> {
    let mut json = false;
    let mut deny_warnings = false;
    let mut paths = Vec::new();
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--json" => json = true,
            "--deny-warnings" => deny_warnings = true,
            "-h" | "--help" => {
                println!(
                    "usage: grant-lint [--json] [--deny-warnings] [FILE|DIR ...]\n\
                     Warnings and advisory hints are non-blocking by default."
                );
                return Ok(0);
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option {argument:?}"));
            }
            _ => paths.push(PathBuf::from(argument)),
        }
    }
    if paths.is_empty() {
        paths.extend([PathBuf::from("std"), PathBuf::from("examples")]);
    }

    let files = collect_glia_files(&paths)?;
    let mut diagnostics = Vec::new();
    for file in files {
        diagnostics.extend(lint_path(&file)?);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&diagnostics)
                .map_err(|error| format!("encode diagnostics: {error}"))?
        );
    } else {
        for diagnostic in &diagnostics {
            println!(
                "{}:{}: {:?} {}: {}\n  risk: {}\n  fix: {}\n  suppression: {}",
                diagnostic.path.display(),
                diagnostic.line,
                diagnostic.severity,
                diagnostic.rule,
                diagnostic.found,
                diagnostic.risk,
                diagnostic.fix,
                diagnostic.suppression
            );
        }
        println!(
            "grant-lint: {} diagnostic(s); runtime confinement does not depend on lint compliance",
            diagnostics.len()
        );
    }

    let has_errors = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error);
    let has_warnings = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Warning);
    Ok(i32::from(has_errors || (deny_warnings && has_warnings)))
}
