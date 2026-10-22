use super::*;

const IMAGE: &[u8] = b"\x1b_Ga=T,f=32,t=d,i=7,p=3,s=1,v=1,c=1,r=1,q=2;/wAA/w==\x1b\\";

fn terminal_render(receiver: &std::sync::mpsc::Receiver<Vec<u8>>, context: &str) -> Vec<u8> {
    match read_server_message(
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect(context),
    ) {
        ServerMessage::Terminal(frame) => frame.bytes,
        other => panic!("expected terminal frame, got {other:?}"),
    }
}

fn attach_test_server(
    initial: &[u8],
    graphics: bool,
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
) {
    let (mut server, control, render, pane) = retained_test_server_with_control(initial);
    let id = server.app.state.workspaces[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let runtime = server.app.state.workspaces[0]
        .test_runtimes
        .remove(&pane)
        .unwrap();
    server.app.terminal_runtimes.insert(id.clone(), runtime);
    server.app.state.ensure_test_terminals();
    let terminal_id = id.to_string();
    let client = server.clients.get_mut(&1).unwrap();
    client.terminal_graphics_enabled = graphics;
    client.mode = ClientConnectionMode::TerminalAttach {
        terminal_id: terminal_id.clone(),
    };
    client.render_state =
        crate::server::render_stream::ClientRenderState::new(RenderEncoding::TerminalAnsi);
    client.cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    };
    (server, control, render, pane)
}

#[tokio::test]
async fn terminal_attach_delivers_graphics_only_changes_and_recovers_after_backpressure() {
    let (mut server, _control, render, pane) = attach_test_server(b"text", true);
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane)
        .unwrap()
        .to_string();
    server.render_and_stream();
    let _ = terminal_render(&render, "text baseline");

    write_shared_test_pane(&mut server, pane, IMAGE);
    server.render_and_stream();
    let uploaded = terminal_render(&render, "graphics-only upload");
    assert!(
        uploaded.windows(3).any(|bytes| bytes == b"\x1b_G"),
        "attach dropped the Kitty APC"
    );
    assert!(String::from_utf8_lossy(&uploaded).contains("/wAA/w=="));
    // The cursor must already be right when the atomic frame ends, before any optional IME anchor.
    let sync_end = crate::protocol::render_ansi::final_sync_output_end(&uploaded).unwrap()
        + b"\x1b[?2026l".len();
    let replayed =
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, &uploaded[..sync_end]);
    let area = Rect::new(0, 0, 80, 24);
    let reference = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap()
        .cursor_state(area, true)
        .unwrap();
    let shown = replayed.cursor_state(area, true).unwrap();
    assert_eq!(
        (shown.x, shown.y),
        (reference.x, reference.y),
        "graphics output moved the terminal cursor"
    );

    write_shared_test_pane(&mut server, pane, b"\x1b_Ga=d,d=A\x1b\\");
    server.render_and_stream();
    let removed = terminal_render(&render, "graphics removal");
    assert!(
        String::from_utf8_lossy(&removed).contains("a=d"),
        "image removal was not delivered"
    );

    // A rejected render must not advance the image-delivery baseline.
    let writer = server.clients[&1].writer.as_ref().unwrap();
    assert!(writer.render.try_send(b"occupied".to_vec()).is_ok());
    while writer.render.try_send(b"occupied".to_vec()).is_ok() {}
    write_shared_test_pane(&mut server, pane, IMAGE);
    server.render_and_stream();
    while render.try_recv().is_ok() {}
    server.render_and_stream();
    let retried = terminal_render(&render, "upload retry");
    assert!(String::from_utf8_lossy(&retried).contains("/wAA/w=="));

    // A newly joined observer needs its own pixels, not just the operator's placement delta.
    let (writer, _control, observed) = test_client_writer();
    let mut observer = ClientConnection::new(
        (80, 24),
        crate::kitty_graphics::HostCellSize {
            width_px: 8,
            height_px: 16,
        },
        2,
        RenderEncoding::TerminalAnsi,
        Some(writer),
    );
    observer.terminal_graphics_enabled = true;
    observer.mode = ClientConnectionMode::TerminalObserve { terminal_id };
    server.clients.insert(2, observer);
    server.render_and_stream();
    let joined = terminal_render(&observed, "observer join");
    assert!(String::from_utf8_lossy(&joined).contains("/wAA/w=="));
}

#[tokio::test]
async fn terminal_graphics_requires_opt_in_and_can_be_disabled_without_changing_the_codec() {
    let (mut server, _control, render, _pane) = attach_test_server(IMAGE, false);
    server.render_and_stream();
    let legacy = terminal_render(&render, "legacy terminal frame");
    assert!(!legacy.windows(3).any(|bytes| bytes == b"\x1b_G"));
    assert!(
        server.handle_server_event(ServerEvent::ClientTerminalGraphics {
            client_id: 1,
            enabled: true
        })
    );
    server.render_and_stream();
    let enabled = terminal_render(&render, "negotiated graphics");
    assert!(String::from_utf8_lossy(&enabled).contains("/wAA/w=="));
    assert!(
        server.handle_server_event(ServerEvent::ClientTerminalGraphics {
            client_id: 1,
            enabled: false
        })
    );
    server.render_and_stream();
    let disabled = terminal_render(&render, "graphics cleanup");
    assert!(String::from_utf8_lossy(&disabled).contains("a=d"));
    assert!(!String::from_utf8_lossy(&disabled).contains("/wAA/w=="));
}

#[tokio::test]
#[ignore = "manual fixed-geometry render scaling profile"]
async fn render_scale_profile_terminal_attach_graphics() {
    for count in [1, 15] {
        for graphics in [false, true] {
            let (mut server, _control, render, pane) = attach_test_server(b"text", graphics);
            for _ in 1..count {
                let workspace = &mut server.app.state.workspaces[0];
                let added = workspace.test_split(ratatui::layout::Direction::Horizontal);
                workspace.insert_test_runtime(
                    added,
                    crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                        80,
                        24,
                        1024 * 1024,
                        &b"populated history\r\n".repeat(2000),
                    ),
                );
            }
            server.app.state.ensure_test_terminals();
            write_shared_test_pane(&mut server, pane, IMAGE);
            server.render_and_stream();
            while render.try_recv().is_ok() {}
            let mut samples = Vec::new();
            for iteration in 0..45 {
                write_shared_test_pane(
                    &mut server,
                    pane,
                    if iteration % 2 == 0 {
                        b"\x1b[2;1HA"
                    } else {
                        b"\x1b[2;1HB"
                    },
                );
                let started = std::time::Instant::now();
                server.render_and_stream();
                let elapsed = started.elapsed().as_nanos();
                while render.try_recv().is_ok() {}
                if iteration >= 5 {
                    samples.push(elapsed);
                }
            }
            samples.sort_unstable();
            eprintln!(
                "terminal_attach_graphics panes={count} enabled={graphics} median_ns={} p95_ns={}",
                samples[20], samples[37]
            );
        }
    }
}
