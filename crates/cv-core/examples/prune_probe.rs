//! Isolated prune probe: source must be a scratch fixture, never a live harness store.
use anyhow::Result;
use cv_core::prune::{prune_session, PruneOptions};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;

/// Run `f` with the requested-Rust-heap snapshot when the probe-heap feature is on.
fn measure<R>(f: impl FnOnce() -> R) -> (R, Option<cv_testkit::heap::Snapshot>) {
    #[cfg(feature = "probe-heap")]
    {
        let baseline = cv_testkit::heap::begin();
        let out = f();
        (out, Some(cv_testkit::heap::end(baseline)))
    }
    #[cfg(not(feature = "probe-heap"))]
    {
        (f(), None)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        anyhow::bail!("expected: prune_probe SOURCE_PATH [MODE...] (modes: rewind|revive|window|range|drop|thinking|declassify|dry|resources)");
    }
    if args.iter().skip(2).any(|mode| mode == "rewind") {
        let source = cv_core::SessionRef::from_path(std::path::Path::new(&args[1]), cv_core::Harness::Claude);
        let options = cv_core::rewind::RewindOptions {
            at: cv_core::rewind::CutAt::End,
            full: true,
            out_dir: Some(std::path::Path::new(&args[1]).parent().unwrap().join("rewound")),
            new_id: Some("22222222-2222-4222-8222-222222222222".into()),
            dry_run: false,
            thinking: Some(cv_core::emit::ThinkingMode::Native),
            generator: "operation-probe".into(),
            skills_policy: Default::default(),
        };
        let start = std::time::Instant::now();
        let (result, heap) = measure(|| cv_core::rewind::rewind_session(&source, &options));
        let result = result?;
        let elapsed = start.elapsed().as_secs_f64();
        println!("{}", serde_json::json!({"result":result,"seconds":elapsed,"heap":heap}));
        return Ok(());
    }
    let mut options = PruneOptions {
        new_id: Some("22222222-2222-4222-8222-222222222222".into()),
        keep_last: 2,
        ..Default::default()
    };
    for mode in args.iter().skip(2) {
        match mode.as_str() {
            "revive" => options.revive = true,
            "window" => options.window = Some(4000),
            "range" => options.keep_range = Some((3, Some(12))),
            "drop" => options.drop = true,
            "thinking" => options.thinking = true,
            "declassify" => {
                options.declassify = true;
                options.declassify_tokens = vec!["alpha".into(), "beta".into()];
            }
            "dry" => options.dry_run = true,
            "resources" => options.copy_resources = true,
            _ => anyhow::bail!("unknown probe mode {mode}"),
        }
    }
    let start = std::time::Instant::now();
    let (result, heap) = measure(|| prune_session(cv_core::Harness::Claude, std::path::Path::new(&args[1]), &options));
    let result = result?;
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({"result": result, "usage_rewritten":result.usage_rewritten,"revive_tokens":result.revive_tokens,"revive_old_tokens":result.revive_old_tokens,"seconds":elapsed,"heap":heap})
    );
    Ok(())
}
