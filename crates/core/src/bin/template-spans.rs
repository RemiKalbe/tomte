use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use czui_core::chezmoi::{ChezmoiClient, ChezmoiOptions};
use czui_core::cmd::SystemRunner;
use czui_core::template::anchor::{SpanOrigin, anchor};
use czui_core::template::lexer::lex;

fn main() -> ExitCode {
    let Some(target) = std::env::args().nth(1) else {
        eprintln!("usage: template-spans <target-path>");
        return ExitCode::FAILURE;
    };
    let target = PathBuf::from(target);
    let chezmoi = ChezmoiClient::new(Arc::new(SystemRunner), ChezmoiOptions::default());

    let source = match chezmoi.source_path(&target) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: source-path failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let template = match std::fs::read_to_string(&source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", source.display());
            return ExitCode::FAILURE;
        }
    };
    if source.extension().and_then(|e| e.to_str()) != Some("tmpl") {
        println!(
            "{} is not a template — every byte is editable.",
            source.display()
        );
        return ExitCode::SUCCESS;
    }
    let rendered = match chezmoi.cat(&target) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("error: chezmoi cat failed (secret manager?): {e}");
            return ExitCode::FAILURE;
        }
    };
    let segments = match lex(&template) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: lex failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let map = anchor(&template, &segments, &rendered);
    let mut out = String::new();
    for span in &map.spans {
        let text = &rendered[span.range.clone()];
        match span.origin {
            SpanOrigin::Literal {
                repeated: false, ..
            } => out.push_str(text),
            SpanOrigin::Literal { repeated: true, .. } => {
                out.push_str(&format!("⟦R:{text}⟧"));
            }
            SpanOrigin::Action { .. } => out.push_str(&format!("⟦A:{text}⟧")),
            SpanOrigin::Unmapped => out.push_str(&format!("⟦U:{text}⟧")),
        }
    }
    println!("{out}");
    println!(
        "-- literal coverage: {:.0}% editable ({} spans)",
        map.literal_coverage() * 100.0,
        map.spans.len()
    );
    ExitCode::SUCCESS
}
