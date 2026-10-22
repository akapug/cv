//! One shape, one identity: the task-list row every task read surface (CLI, MCP tools,
//! `cv serve` HTTP routes) renders from (the inbox's row is [`super::inbox::InboxItem`]). Field names and declaration order here ARE the wire
//! contract — the workspace builds serde_json with `preserve_order`, so struct field order is the
//! emitted key order, and all three surfaces must stay byte-identical to what they shipped.
//!
//! One surface asymmetry that predates this module is carried as optional fields rather than
//! re-unified (that would break existing consumers): the HTTP task row carries timestamps the MCP
//! row never did ([`TaskRow::full`] vs [`TaskRow::brief`]).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::project;
use super::reduce::TaskProjection;

/// One task-list row (`cv task list`, MCP `task_list`, `GET /api/tasks`).
#[derive(Clone, Debug, Serialize)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub effective_state: String,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    /// Timestamps ride the HTTP row only ([`TaskRow::full`]); the MCP row shipped without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<DateTime<Utc>>,
    /// Tags and blockers ride every surface, omitted when empty — a task that has neither
    /// serializes byte-for-byte as it did before they existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// The backlog columns (`GET /api/tasks?detail=1`, what the inbox page lists); absent elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<bool>,
    /// Blocked by a task that has not finished ([`project::is_blocked`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
}

impl TaskRow {
    /// Every key a row can carry, in emitted order (`cv schema --json` publishes it as
    /// `task_row`): the lean row omits the timestamps and the backlog columns, and `tags` /
    /// `blocked_by` ride only when non-empty.
    pub const KEYS: &'static [&'static str] = &[
        "id",
        "title",
        "effective_state",
        "assignee",
        "repo",
        "opened_at",
        "last_ts",
        "tags",
        "blocked_by",
        "opened_by",
        "issue",
        "body",
        "notes",
        "decision",
        "blocked",
    ];

    /// The lean row (MCP `task_list`): no timestamps on the wire.
    pub fn brief(t: &TaskProjection) -> TaskRow {
        TaskRow {
            id: t.task_id.clone(),
            title: t.title.clone(),
            effective_state: project::effective_display(t),
            assignee: t.assignee.clone(),
            repo: t.repo.clone(),
            opened_at: None,
            last_ts: None,
            tags: t.tags.clone(),
            blocked_by: t.blocked_by.clone(),
            opened_by: None,
            issue: None,
            body: None,
            notes: None,
            decision: None,
            blocked: None,
        }
    }

    /// The timestamped row (`GET /api/tasks`, and the CLI's text rendering ages off `last_ts`).
    pub fn full(t: &TaskProjection) -> TaskRow {
        TaskRow {
            opened_at: Some(t.opened_at),
            last_ts: Some(t.last_ts),
            ..TaskRow::brief(t)
        }
    }

    /// The timestamped row plus the backlog columns (`GET /api/tasks?detail=1`).
    pub fn detailed(t: &TaskProjection, model: &super::reduce::TaskReadModel) -> TaskRow {
        TaskRow {
            opened_by: Some(t.opened_by.clone()),
            issue: t.issue.clone(),
            body: Some(t.body.clone()),
            notes: Some(t.notes.len()),
            decision: Some(t.decision.is_some()),
            blocked: Some(project::is_blocked(model, t)),
            ..TaskRow::full(t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{TaskEvent, TaskEventKind};
    use crate::task::reduce::{TaskReadModel, TaskReducer};

    fn model_with_claimed_task() -> (TaskReadModel, String) {
        let ts: DateTime<Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
        let open = TaskEvent {
            id: "00000000-0000-7000-8000-000000000001".into(),
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            ts,
            by: "h".into(),
            kind: TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: Some(PathBuf::from("/tmp/r")),
                issue: None,
                channel: None,
                assignee: None,
            },
        };
        let id = open.task_id.clone();
        let claim = TaskEvent {
            id: "00000000-0000-7000-8000-000000000002".into(),
            task_id: id.clone(),
            ts,
            by: "agent:a".into(),
            kind: TaskEventKind::Claimed {
                assignee: "agent:a".into(),
            },
        };
        (TaskReducer::reduce(&[open, claim]).unwrap(), id)
    }

    /// The wire contract: key names AND order are frozen per surface (preserve_order means
    /// declaration order is emitted order).
    #[test]
    fn row_serialization_is_the_shipped_wire_shape() {
        let (model, id) = model_with_claimed_task();
        let t = &model.tasks[&id];

        let brief = serde_json::to_value(TaskRow::brief(t)).unwrap();
        assert_eq!(
            serde_json::to_string(&brief).unwrap(),
            format!(r#"{{"id":"{id}","title":"t","effective_state":"claimed","assignee":"agent:a","repo":"/tmp/r"}}"#),
        );
        let full = serde_json::to_value(TaskRow::full(t)).unwrap();
        let keys: Vec<&str> = full.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "title",
                "effective_state",
                "assignee",
                "repo",
                "opened_at",
                "last_ts"
            ],
        );
    }

    /// A detailed row with every optional column set emits exactly `TaskRow::KEYS`, in order.
    #[test]
    fn keys_are_the_serde_key_set() {
        let (model, id) = model_with_claimed_task();
        let row = TaskRow {
            issue: Some("#1".into()),
            tags: vec!["t".into()],
            blocked_by: vec!["b".into()],
            ..TaskRow::detailed(&model.tasks[&id], &model)
        };
        let value = serde_json::to_value(row).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, TaskRow::KEYS);
    }
}
