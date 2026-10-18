//! `cv schema` — the reference. Bare: the `-q` query-calculus reference (`cv_core::query`).
//! `--json`: the machine-readable schema of every shape cv emits (session row, session, message,
//! block, query). `--commands`: the command tree — with `--json` in the exact shape `cv mcp`
//! generates its tools from (docs/INTERFACE-V2.md §6), produced from clap's own introspection so
//! names, flags, and help can't drift from the binary.

use anyhow::Result;
use cv_core::query;
use serde_json::{json, Value};
use std::any::TypeId;
use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) fn cmd_schema(cli: &clap::Command, json: bool, commands: bool) -> Result<()> {
    if commands {
        if json {
            println!("{}", serde_json::to_string_pretty(&commands_json(cli))?);
        } else {
            for (group, names) in crate::GROUPS {
                println!("{group}");
                for name in names.iter() {
                    if let Some(sub) = cli.get_subcommands().find(|s| s.get_name() == *name) {
                        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
                        println!("  {name:<11} {about}");
                    }
                }
            }
            println!("\n(`cv schema --commands --json` for the full tree with every argument)");
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&data_schema_json())?);
    } else {
        print!("{}", query::reference());
        println!(
            "\nMachine-readable: `cv schema --json` (data shapes) · `cv schema --commands --json` (the command tree)"
        );
    }
    Ok(())
}

/// Every `MessageKind` as serde spells it. Kept in step with `cv_core::ir::MessageKind` by
/// `kinds_and_origins_round_trip` (each name must deserialize).
pub(crate) const MESSAGE_KINDS: &[&str] = &[
    "prompt",
    "reply",
    "tool_result",
    "injected_context",
    "system_prompt",
    "notice",
    "compaction_boundary",
    "compaction_summary",
    "model_change",
    "error",
    "subagent_spawn",
    "subagent_return",
    "branch",
    "carrier",
];

/// Every `Origin` as serde spells it.
pub(crate) const ORIGINS: &[&str] = &[
    "human",
    "model",
    "harness",
    "hook",
    "scheduler",
    "subagent",
    "import",
    "unknown",
];

pub(crate) const ROLES: &[&str] = &["system", "user", "assistant", "tool"];

/// The data shapes: what `--json` outputs are made of. Field lists mirror `cv_core::ir`; the
/// block `type` tag and the message `kind`/`origin` vocabularies are enumerated so a consumer can
/// branch without reading Rust.
pub(crate) fn data_schema_json() -> Value {
    json!({
        "session_row": {
            "description": "one session in a list — identical across `ls --json` and `search --json`",
            "keys": ["id", "harness", "path", "cwd", "title", "created_at", "updated_at", "message_count", "size_bytes"],
            "enrich_keys": ["display_title", "git"],
            "search_extra_keys": ["score", "snippet", "agent_id", "parent_id", "workflow"],
            "timestamps": "RFC 3339 strings, null when unknown",
        },
        "session": {
            "description": "the IR `Session` (`show --json`, `export --format json`)",
            "fields": ["id", "harness", "principal", "cwd", "title", "created_at", "updated_at", "model", "git",
                       "system_prompt", "lineage", "messages", "source_path", "extra"],
            "principal": "optional opaque string or object; missing/null means unspecified",
            "lineage": ["forked_from", "parent", "spawned_by_tool_use", "continued_in", "continues", "agent_path"],
            "extra": "harness-specific facts nested under the harness name: extra[\"claude\"][…]; never flat",
        },
        "message": {
            "fields": ["id", "parent_id", "role", "kind", "origin", "timestamp", "model", "content", "usage", "extra"],
            "role": ROLES,
            "kind": MESSAGE_KINDS,
            "origin": ORIGINS,
            "usage": ["input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens",
                      "reasoning_tokens", "cost_usd"],
        },
        "block": {
            "tag": "type",
            "types": {
                "text": ["text"],
                "thinking": ["text", "signature", "encrypted", "redacted"],
                "tool_use": ["id", "name", "input", "namespace"],
                "tool_result": ["tool_use_id", "content", "is_error", "tool_name", "status", "details"],
                "image": ["media_type", "data_ref"],
                "file": ["mime", "path", "source"],
            },
        },
        "prompt_row": {
            "description": "one row of `show --kind prompt,answer --json`: what the person typed, or an AskUserQuestion answer",
            "keys": ["index", "timestamp", "kind", "text"],
            "kind": ["prompt", "answer"],
        },
        "lane": {
            "description": "one sub-agent in `lanes --json`, launch order",
            "keys": ["agent_id", "session_id", "path", "agent_type", "description", "tool_use_id", "workflow", "model",
                     "started_at", "last_turn_at", "duration_ms", "messages", "tool_calls", "tokens", "status",
                     "status_source", "last_text", "last_tool", "stranded", "failure_cause", "resets_at", "failure_detail",
                     "endpoint", "endpoint_source", "tasks"],
            "tokens": ["calls", "input", "cache_read", "cache_write", "output", "total"],
            "status": "running · returned · completed · stopped · failed · killed · or a journaled workflow status (done / partial / …)",
            "status_source": ["journal", "task_notification", "subagent_stop", "hook_ledger", "transcript"],
            "stranded": "status completed/stopped AND the last text's final sentences say it is waiting",
            "failure_cause": "failed/killed lanes only, from the transcript's last uncleared API-error notice: rate-limited · context · stopped (absent = undetermined)",
            "endpoint": "the CV_ENDPOINT the lane's own shell commands exported first (lane:<name>; an Agent prompt or a written brief does not count), else with --tasks a lane:<slug> assignee matching the description's leading token",
            "endpoint_source": ["transcript", "description"],
            "tasks": "with --tasks: [{id, title, state, last_note?, last_ts}] the endpoint holds (assignee), newest first",
            "parent_id": "with --recent only: the session whose forest the lane belongs to",
        },
        "deferral": {
            "description": "one row of `deferrals --json`: a place the assistant put something off",
            "keys": ["index", "timestamp", "phrase", "context", "matched"],
            "phrase": "the label of the phrase that fired — the built-in table's labels are listed under `default_phrases`; `--phrases-file` (or `<config>/deferrals.txt`) replaces the table with your own",
            "default_phrases": crate::cmd::orchestrate::default_phrase_labels(),
            "matched": "with --open-tasks: {task_id, title, state, shared[]} or absent when no task shares ≥3 significant words",
        },
        "query": query::schema_json(),
    })
}

/// The command tree in the §6 shape: one entry per visible command and subcommand
/// (`"task open"`), each `{ name, group, about, args: [{ name, kind, value_type, possible_values?,
/// default?, help, required }] }`.
pub(crate) fn commands_json(cli: &clap::Command) -> Value {
    // Built, so the groups an `#[arg(group = …)]` names exist on each command (see push_command).
    let mut cli = cli.clone();
    cli.build();
    let mut out = Vec::new();
    for sub in cli.get_subcommands().filter(|s| visible(s)) {
        let group = crate::group_of(sub.get_name()).unwrap_or("");
        push_command(&mut out, sub, None, group);
    }
    Value::Array(out)
}

fn push_command(out: &mut Vec<Value>, cmd: &clap::Command, parent: Option<&str>, group: &str) {
    let name = match parent {
        Some(p) => format!("{p} {}", cmd.get_name()),
        None => cmd.get_name().to_string(),
    };
    // An argument in a group is never required on its own: `required` on a grouped arg (`task
    // resolve --choice` in the `answer` group) means "one of the group", which clap enforces and a
    // per-argument `required` would misstate as "always this one". The derive's own group per
    // variant holds every argument; that one says nothing and is skipped.
    let all_ids: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
    let grouped: Vec<&str> = cmd
        .get_groups()
        .filter(|g| g.get_args().count() < all_ids.iter().filter(|id| !matches!(**id, "help" | "version")).count())
        .flat_map(|g| g.get_args().map(|id| id.as_str()))
        .collect();
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(|a| {
            let mut v = arg_json(a);
            if grouped.contains(&a.get_id().as_str()) {
                v["required"] = json!(false);
            }
            v
        })
        .collect();
    out.push(json!({
        "name": name,
        "group": group,
        "about": cmd.get_about().map(|a| a.to_string()).unwrap_or_default(),
        "args": args,
    }));
    for sub in cmd.get_subcommands().filter(|s| visible(s)) {
        push_command(out, sub, Some(&name), group);
    }
}

/// A subcommand the tree reports: not hidden, and not the `help` clap's build adds to every parent.
fn visible(cmd: &clap::Command) -> bool {
    !cmd.is_hide_set() && cmd.get_name() != "help"
}

fn arg_json(a: &clap::Arg) -> Value {
    let takes_values = a.get_action().takes_values();
    let kind = if a.is_positional() {
        "positional"
    } else if takes_values {
        "option"
    } else {
        "flag"
    };
    let mut v = json!({
        "name": a.get_id().as_str(),
        "kind": kind,
        "value_type": value_type(a),
        "help": a
            .get_long_help()
            .or(a.get_help())
            .map(|h| h.to_string())
            .unwrap_or_default(),
        "required": a.is_required_set(),
    });
    let obj = v.as_object_mut().expect("json object");
    if let Some(long) = a.get_long() {
        obj.insert("long".into(), json!(long));
    }
    if matches!(a.get_action(), clap::ArgAction::Append) || a.get_num_args().is_some_and(|n| n.max_values() > 1) {
        obj.insert("multiple".into(), json!(true));
    }
    if takes_values {
        let possible: Vec<String> = a
            .get_possible_values()
            .iter()
            .filter(|p| !p.is_hide_set())
            .map(|p| p.get_name().to_string())
            .collect();
        if !possible.is_empty() {
            obj.insert("possible_values".into(), json!(possible));
        }
        if let Some(d) = a.get_default_values().first() {
            obj.insert("default".into(), json!(d.to_string_lossy()));
        }
    }
    v
}

/// The JSON-schema-ish scalar type of an argument's values, from clap's value parser.
fn value_type(a: &clap::Arg) -> &'static str {
    if !a.get_action().takes_values() {
        return "boolean";
    }
    let t = a.get_value_parser().type_id();
    if t == TypeId::of::<PathBuf>() {
        "path"
    } else if t == TypeId::of::<String>() || t == TypeId::of::<OsString>() {
        "string"
    } else if t == TypeId::of::<bool>() {
        "boolean"
    } else if t == TypeId::of::<usize>()
        || t == TypeId::of::<u64>()
        || t == TypeId::of::<u32>()
        || t == TypeId::of::<u16>()
        || t == TypeId::of::<u8>()
        || t == TypeId::of::<i64>()
        || t == TypeId::of::<i32>()
        || t == TypeId::of::<i16>()
        || t == TypeId::of::<i8>()
    {
        "integer"
    } else if t == TypeId::of::<f64>() || t == TypeId::of::<f32>() {
        "number"
    } else {
        "string"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::{MessageKind, Origin, Role};

    /// The enumerated vocabularies match the IR's serde spelling (a renamed/removed variant
    /// fails here; a new one must be added to the list to be published).
    #[test]
    fn kinds_and_origins_round_trip() {
        for k in MESSAGE_KINDS {
            serde_json::from_value::<MessageKind>(json!(k)).unwrap_or_else(|e| panic!("kind {k}: {e}"));
        }
        for o in ORIGINS {
            serde_json::from_value::<Origin>(json!(o)).unwrap_or_else(|e| panic!("origin {o}: {e}"));
        }
        for r in ROLES {
            serde_json::from_value::<Role>(json!(r)).unwrap_or_else(|e| panic!("role {r}: {e}"));
        }
        // The default-by-role table the contract fixes.
        assert_eq!(MessageKind::for_role(Role::User), MessageKind::Prompt);
        assert_eq!(MessageKind::for_role(Role::System), MessageKind::Notice);
    }

    #[test]
    fn prompt_row_reference_names_a_live_command() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let schema = data_schema_json();
                let description = schema["prompt_row"]["description"].as_str().unwrap();
                let command = description.split('`').nth(1).unwrap();
                let mut tree = crate::build_cli();
                tree.build();
                let mut words = command.split_whitespace();
                let name = words.next().unwrap();
                assert!(tree.get_subcommands().any(|c| c.get_name() == name && !c.is_hide_set()));
                tree.try_get_matches_from(["cv", name, "session-id"].into_iter().chain(words))
                    .unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// `docs/INTERFACE-V2.md` §1 (the command groups) and §6 (the MCP roster) are generated from
    /// the clap tree between `<!-- begin: … -->`/`<!-- end: … -->` markers, so the contract cannot
    /// state a command set the binary does not have. `CV_REGEN_CONTRACT=1` rewrites them.
    #[test]
    fn contract_tables_are_the_clap_tree() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // a debug clap tree overflows the 2 MB test stack
            .spawn(contract_tables_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn contract_tables_body() {
        let tree = crate::build_cli();
        let commands = commands_json(&tree);
        let commands = commands.as_array().unwrap();
        let name = |c: &Value| c["name"].as_str().unwrap().to_string();
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        let mut subs: Vec<(String, Vec<String>)> = Vec::new();
        for c in commands {
            let (n, g) = (name(c), c["group"].as_str().unwrap().to_string());
            match n.split_once(' ') {
                Some((parent, sub)) => match subs.iter_mut().find(|(p, _)| p == parent) {
                    Some((_, v)) => v.push(sub.to_string()),
                    None => subs.push((parent.to_string(), vec![sub.to_string()])),
                },
                None => match groups.iter_mut().find(|(have, _)| *have == g) {
                    Some((_, v)) => v.push(n),
                    None => groups.push((g, vec![n])),
                },
            }
        }
        let ticks = |v: &[String]| v.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" ");
        let mut s1 = String::from("| group | commands |\n|---|---|\n");
        for (g, v) in &groups {
            s1.push_str(&format!("| **{g}** | {} |\n", ticks(v)));
        }
        s1.push_str("\n| command | subcommands |\n|---|---|\n");
        for (p, v) in &subs {
            s1.push_str(&format!("| `{p}` | {} |\n", ticks(v)));
        }

        let group_of = |tool: &str| {
            commands
                .iter()
                .find(|c| name(c).replace(' ', "_") == tool)
                .map(|c| c["group"].as_str().unwrap().to_string())
                .unwrap_or_else(|| "MCP-only".to_string())
        };
        let generated = crate::cmd::mcp_tools::CliTools::from_tree(&tree).unwrap().tool_list();
        let hand = crate::cmd::mcp::base_tool_list();
        let mut roster: Vec<(String, Vec<String>)> = Vec::new();
        for t in generated.iter().chain(hand.as_array().unwrap()) {
            let tool = t["name"].as_str().unwrap().to_string();
            let g = group_of(&tool);
            match roster.iter_mut().find(|(have, _)| *have == g) {
                Some((_, v)) => v.push(tool),
                None => roster.push((g, vec![tool])),
            }
        }
        let mut s6 = String::from("| group | tools |\n|---|---|\n");
        for (g, v) in &roster {
            s6.push_str(&format!("| **{g}** | {} |\n", ticks(v)));
        }

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/INTERFACE-V2.md");
        let doc = std::fs::read_to_string(&path).unwrap();
        let want = splice_marked(
            &splice_marked(&doc, "cv schema --commands", &s1),
            "cv mcp tools/list",
            &s6,
        );
        if std::env::var_os("CV_REGEN_CONTRACT").is_some() {
            std::fs::write(&path, &want).unwrap();
        } else {
            assert!(
                doc == want,
                "docs/INTERFACE-V2.md §1/§6 disagree with the clap tree; regenerate with\n  \
                 CV_REGEN_CONTRACT=1 cargo test -p clustervision --bin cv contract_tables\n\
                 and review the diff (a command that appeared or went is a contract change)"
            );
        }
    }

    /// `doc` with the text between `<!-- begin: {key} -->` and `<!-- end: {key} -->` replaced.
    fn splice_marked(doc: &str, key: &str, body: &str) -> String {
        let (begin, end) = (format!("<!-- begin: {key} -->\n"), format!("<!-- end: {key} -->"));
        let a = doc.find(&begin).unwrap_or_else(|| panic!("no `{begin}` marker")) + begin.len();
        let b = doc[a..].find(&end).unwrap_or_else(|| panic!("no `{end}` marker")) + a;
        format!("{}{body}{}", &doc[..a], &doc[b..])
    }

    #[test]
    fn data_schema_names_the_session_row_keys() {
        let v = data_schema_json();
        assert_eq!(v["block"]["tag"], "type");
        assert_eq!(v["session_row"]["keys"].as_array().unwrap().len(), 9);
        assert!(v["query"].is_object());
        assert!(v["lane"]["status_source"]
            .as_array()
            .unwrap()
            .contains(&json!(cv_core::lanes::StatusSource::HookLedger)));
    }
}
