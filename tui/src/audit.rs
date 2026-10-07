//! Cross-view rendering gates for the workbench plan.
#[cfg(test)]
mod tests {
    use crate::{
        agent_info,
        app::{App, SessionView},
        context, references, transcript,
    };
    use agentmux_core::{AgentId, Event, EventKind, Session, SessionId, SessionState, WorkspaceId};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    fn fixture() -> App {
        let session = Session {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("mock"),
            state: SessionState::Ready,
            acp_session_id: None,
            native_session_file: None,
            native_terminal: false,
            references: vec![],
            created_at: Utc::now(),
        };
        let id = session.id;
        let mut app = App::new(
            vec![],
            vec![],
            vec![SessionView {
                session,
                agent_name: "Mock".into(),
                workspace_name: "检查空间".into(),
            }],
            vec![],
        );
        for (seq, value) in [
            (
                1,
                serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"text":"检查中文界面与所有控制状态"}}),
            ),
            (
                2,
                serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"text":"### 检查结果\n\n中文查询与原始代码保持一致。\n\n```rust\n    let value = 42;\n```\n"}}),
            ),
            (
                3,
                serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Read configuration","rawOutput":"Tool details","status":"completed"}),
            ),
        ] {
            app.handle_event(Event {
                session_id: id,
                seq,
                ts: Utc::now(),
                kind: EventKind::SessionUpdate(value),
            });
        }
        app.insert_text("尚未发送的中文草稿👩‍💻");
        let workspace = app.file_workspace().unwrap();
        app.wb.context.snapshots.entry(workspace).or_default().data =
            Some(agentmux_core::rpc::WorkspaceContextResult {
                context: Some("# 共享上下文\n\n测试协作信息与输入保持分离。\n".into()),
                activity: Some("近期活动\n测试 Agent 完成检查\n".into()),
                ..Default::default()
            });
        app
    }

    #[test]
    fn all_views_fit_and_render_nonblank_at_required_sizes() {
        let output = std::env::var_os("AGENTMUX_AUDIT_DIR").map(std::path::PathBuf::from);
        if let Some(output) = &output {
            std::fs::create_dir_all(output).unwrap();
        }
        for (width, height) in [(20, 8), (40, 16), (80, 24), (120, 30), (140, 38), (160, 40)] {
            for view in [
                "chat",
                "picker",
                "messages",
                "reader",
                "info",
                "context",
                "references",
                "diff",
            ] {
                let mut app = fixture();
                app.wb.columns = width;
                let id = app.selected_session_id().unwrap();
                match view {
                    "picker" => app.open_picker(),
                    "messages" => transcript::open(&mut app),
                    "reader" => transcript::choose(&mut app, id, 2),
                    "info" => agent_info::open(&mut app),
                    "context" => context::open(&mut app),
                    "references" => references::open(&mut app),
                    "diff" => {
                        app.wb.inspection = Some((
                            "中文文件.rs".into(),
                            "diff --git a/file b/file\n@@ first @@\n-old\n+new\n".into(),
                        ));
                        crate::focus::select(&mut app, crate::focus::Pane::Reading);
                    }
                    _ => {}
                }
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| crate::shell::draw(frame, &app))
                    .unwrap();
                let frame = Rect::new(0, 0, width, height);
                assert!(
                    app.wb
                        .hits
                        .borrow()
                        .iter()
                        .all(|hit| hit.area.intersection(frame) == hit.area),
                    "{view} {width}x{height}: hit outside frame"
                );
                let buffer = terminal.backend().buffer();
                assert!(
                    buffer
                        .content
                        .iter()
                        .any(|cell| !cell.symbol().trim().is_empty()),
                    "{view} blank"
                );
                assert!(
                    buffer
                        .content
                        .iter()
                        .all(|cell| !cell.symbol().contains(['\n', '\r'])),
                    "{view} has multiline cells"
                );
                assert_eq!(app.input, "尚未发送的中文草稿👩‍💻");
                if let Some(output) = &output {
                    let cells:Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg),"bold":cell.modifier.contains(ratatui::style::Modifier::BOLD)})).collect();
                    std::fs::write(
                        output.join(format!("{view}-{width}-{height}.json")),
                        serde_json::to_vec(
                            &serde_json::json!({"width":width,"height":height,"cells":cells}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
            }
        }
    }
}
