//! Integration tests for the shared-workspace collaboration layer:
//! `.agentmux/context.md` (the blackboard), `.agentmux/activity.md` (the
//! running log) and the preamble injected into sibling sessions.

use std::path::PathBuf;

use agentmux_core::collab::{
    append_activity, init_shared_dir, shared_context_preamble, summarize_event,
};
use agentmux_core::model::{EventKind, SessionState};
use tempfile::TempDir;

fn activity_log(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join(".agentmux/activity.md")).unwrap()
}

#[test]
fn init_shared_dir_creates_agentmux_dir_and_both_files() {
    let dir = tempfile::tempdir().unwrap();

    init_shared_dir(dir.path()).expect("init on a fresh worktree should succeed");

    let shared = dir.path().join(".agentmux");
    assert!(shared.is_dir(), ".agentmux/ should exist");
    assert!(shared.join("context.md").is_file());
    assert!(shared.join("activity.md").is_file());

    // context.md ships a template header explaining the blackboard;
    // activity.md starts empty.
    let context = std::fs::read_to_string(shared.join("context.md")).unwrap();
    assert!(
        !context.trim().is_empty(),
        "context.md should carry a template header"
    );
    assert_eq!(activity_log(&dir), "", "activity.md should start empty");
}

#[test]
fn init_shared_dir_is_idempotent_and_never_wipes_content() {
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();

    let context_path = dir.path().join(".agentmux/context.md");
    std::fs::write(&context_path, "alice owns src/a.rs\n").unwrap();
    append_activity(dir.path(), "alice", "did a thing").unwrap();

    init_shared_dir(dir.path()).expect("second init should succeed");

    assert_eq!(
        std::fs::read_to_string(&context_path).unwrap(),
        "alice owns src/a.rs\n",
        "re-init must not overwrite context.md"
    );
    assert_eq!(
        activity_log(&dir).lines().count(),
        1,
        "re-init must not truncate activity.md"
    );
}

#[test]
fn append_activity_appends_timestamped_lines() {
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();

    append_activity(dir.path(), "alice", "edited src/main.rs").unwrap();
    append_activity(dir.path(), "bob", "ran the test suite").unwrap();

    let lines: Vec<String> = activity_log(&dir).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "each call appends exactly one line");

    // Format: `- [YYYY-MM-DD HH:MM:SS UTC] <agent>: <summary>`
    assert!(lines[0].starts_with("- ["), "got: {}", lines[0]);
    assert!(
        lines[0].contains(" UTC] alice: edited src/main.rs"),
        "got: {}",
        lines[0]
    );
    assert!(
        lines[1].contains(" UTC] bob: ran the test suite"),
        "got: {}",
        lines[1]
    );
}

#[test]
fn summarize_event_maps_file_edits_and_ignores_the_rest() {
    assert_eq!(
        summarize_event(&EventKind::FileEdited {
            path: PathBuf::from("src/main.rs"),
        }),
        Some("edited src/main.rs".to_string())
    );
    assert_eq!(
        summarize_event(&EventKind::FileEdited {
            path: PathBuf::from("/abs/f.rs"),
        }),
        Some("edited /abs/f.rs".to_string())
    );

    let ignored = [
        EventKind::StateChanged {
            from: SessionState::Ready,
            to: SessionState::Prompting,
        },
        EventKind::AgentExited { code: Some(0) },
        EventKind::Orchestrator("note".into()),
        EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"},
        })),
    ];
    for kind in ignored {
        assert_eq!(
            summarize_event(&kind),
            None,
            "{kind:?} should not produce an activity line"
        );
    }
}

#[test]
fn summarize_event_maps_tool_call_updates_when_a_path_is_visible() {
    let kind = EventKind::SessionUpdate(serde_json::json!({
        "sessionUpdate": "tool_call",
        "kind": "edit",
        "title": "Edit src/lib.rs",
        "locations": [{"path": "src/lib.rs"}],
    }));
    let summary = summarize_event(&kind).expect("tool_call edit should summarize");
    assert!(
        summary.contains("src/lib.rs"),
        "summary should name the edited file, got: {summary}"
    );
}

#[test]
fn preamble_is_none_for_a_lone_session() {
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();
    append_activity(dir.path(), "alice", "edited src/a.rs").unwrap();

    assert_eq!(
        shared_context_preamble(dir.path(), 1).unwrap(),
        None,
        "a single session never needs the shared preamble"
    );
    assert_eq!(shared_context_preamble(dir.path(), 0).unwrap(), None);
}

#[test]
fn preamble_is_none_when_the_blackboard_has_no_content() {
    // No .agentmux dir at all → nothing to inject.
    let bare = tempfile::tempdir().unwrap();
    assert_eq!(shared_context_preamble(bare.path(), 3).unwrap(), None);

    // .agentmux exists but both files are blank → still nothing to say.
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();
    std::fs::write(dir.path().join(".agentmux/context.md"), "  \n").unwrap();
    assert_eq!(shared_context_preamble(dir.path(), 2).unwrap(), None);
}

#[test]
fn preamble_includes_context_activity_and_instructions() {
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();
    std::fs::write(
        dir.path().join(".agentmux/context.md"),
        "alice owns src/a.rs\n",
    )
    .unwrap();
    append_activity(dir.path(), "alice", "edited src/a.rs").unwrap();
    append_activity(dir.path(), "bob", "edited src/b.rs").unwrap();

    let preamble = shared_context_preamble(dir.path(), 2)
        .unwrap()
        .expect("content + 2 sessions should yield a preamble");

    assert!(
        preamble.contains("shared workspace"),
        "intro should mention the shared workspace, got:\n{preamble}"
    );
    assert!(
        preamble.contains("alice owns src/a.rs"),
        "context.md contents should be included, got:\n{preamble}"
    );
    assert!(
        preamble.contains("alice: edited src/a.rs"),
        "activity lines should be included, got:\n{preamble}"
    );
    assert!(preamble.contains("bob: edited src/b.rs"));
    assert!(
        preamble.to_lowercase().contains("other agents"),
        "preamble should warn about other agents' files, got:\n{preamble}"
    );
}

#[test]
fn preamble_keeps_only_the_last_20_activity_lines() {
    let dir = tempfile::tempdir().unwrap();
    init_shared_dir(dir.path()).unwrap();
    for i in 0..25 {
        append_activity(dir.path(), "a", &format!("line {i}")).unwrap();
    }

    let preamble = shared_context_preamble(dir.path(), 2).unwrap().unwrap();
    assert!(preamble.contains("a: line 24"));
    assert!(preamble.contains("a: line 5"), "20 lines means 5..=24");
    assert!(
        !preamble.contains("a: line 4"),
        "older entries should be dropped, got:\n{preamble}"
    );
    assert!(!preamble.contains("a: line 0"));
}
