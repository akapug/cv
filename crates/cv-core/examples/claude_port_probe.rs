//! Scratch-only verified native port (Claude by default; optional final target argument).
use anyhow::{Context, Result};
use cv_core::{
    emit::{self, EmitOptions, ThinkingMode},
    Harness, SessionRef,
};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
#[cfg(feature = "sqlite")]
#[path = "store_probe/sqlite_heap.rs"]
mod sqlite_heap;
fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args().collect();
    #[cfg(not(feature = "sqlite"))]
    if args.iter().any(|a| a == "--sqlite-heap") {
        anyhow::bail!("--sqlite-heap requires the sqlite feature");
    }
    #[cfg(feature = "sqlite")]
    let sqlite_accounting = args
        .iter()
        .any(|a| a == "--sqlite-heap")
        .then(sqlite_heap::Accounting::enable)
        .transpose()?;
    args.retain(|a| a != "--sqlite-heap");
    let positional = |index: usize| -> Result<&String> {
        args.get(index)
            .with_context(|| format!("missing positional {index} (usage: <source> <path> <output> <mode> [target])"))
    };
    let source = Harness::parse(positional(1)?).context("unknown source harness")?;
    let adapter = cv_core::harness::for_harness(source).context("source adapter")?;
    let reference = SessionRef::new("synthetic-source", source, positional(2)?);
    let mode = ThinkingMode::parse(positional(4)?).context("unknown thinking mode")?;
    let target = match args.get(5) {
        Some(value) => Harness::parse(value).context("unknown target harness")?,
        None => Harness::Claude,
    };
    #[cfg(feature = "sqlite")]
    let sqlite_baseline = sqlite_accounting.as_ref().map(|a| a.begin()).transpose()?;
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let session = emit::parse_for_emit(adapter.as_ref(), &reference, target)?;
    let (result, report) = emit::emit_report(
        &session,
        target,
        std::path::Path::new(positional(3)?.as_str()),
        &EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            thinking: mode,
            parsed_at: emit::ParsedAt::for_port(reference.harness, target),
            ..Default::default()
        },
    )?;
    #[cfg(feature = "probe-heap")]
    let heap = cv_testkit::heap::end(baseline);
    println!(
        "{}",
        serde_json::json!({"path":result.path,"messages":session.messages.len(),"fidelity":report})
    );
    #[cfg(feature = "sqlite")]
    if let Some(accounting) = sqlite_accounting {
        eprintln!("{}", serde_json::to_string(&accounting.end(sqlite_baseline.unwrap())?)?);
    }
    #[cfg(feature = "probe-heap")]
    eprintln!("{}", serde_json::to_string(&heap)?);
    Ok(())
}
