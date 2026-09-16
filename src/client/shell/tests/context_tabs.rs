// fork: context tabs
use super::*;
use crate::protocol::ClientShellAgent;
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

fn agent(pane_id: &str, workspace_id: &str, focused: bool) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: format!("{workspace_id}-tab"),
        name: Some(pane_id.into()),
        display_agent: None,
        agent: None,
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused,
    }
}

/// ws_1 (focused, no token), ws_2 kund, ws_3 privat, ws_4 kund; agents on ws_1 and ws_2.
pub(super) fn contextual_snapshot() -> ClientShellSnapshot {
    let mut projected = snapshot();
    let base = projected.workspaces[0].clone();
    projected.workspaces = [None, Some("kund"), Some("privat"), Some("kund")]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            let number = index + 1;
            let mut workspace = base.clone();
            workspace.workspace_id = format!("ws_{number}");
            workspace.active_tab_id = format!("ws_{number}-tab");
            workspace.number = number;
            workspace.label = format!("space-{number}");
            workspace.focused = number == 1;
            workspace.tokens = context
                .map(|context| vec![("context".to_owned(), context.to_owned())])
                .unwrap_or_default();
            workspace
        })
        .collect();
    projected.tabs = projected
        .workspaces
        .iter()
        .map(|workspace| {
            let mut tab = projected.tabs[0].clone();
            tab.tab_id = workspace.active_tab_id.clone();
            tab.workspace_id = workspace.workspace_id.clone();
            tab.focused = workspace.focused;
            tab
        })
        .collect();
    projected.panes[0].tab_id = "ws_1-tab".into();
    projected.focused_tab_id = Some("ws_1-tab".into());
    projected.agents = vec![
        agent("pane_1", "ws_1", true),
        agent("pane_2", "ws_2", false),
    ];
    projected
}

pub(super) fn contextual_state(token: Option<&str>) -> ClientShellState {
    let mut config = Config::default();
    config.ui.sidebar.spaces.context_token = token.map(str::to_owned);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    // Wide enough for "+ default kund privat all" plus the divider column.
    state.sidebar_width = 32;
    state.sidebar_width_manual = true;
    state.set_snapshot(Box::new(contextual_snapshot()));
    state.set_pane_surface(surface());
    state
}

pub(super) fn visible_workspaces(state: &ClientShellState) -> Vec<String> {
    state
        .hits
        .workspaces
        .iter()
        .map(|hit| hit.workspace_id.clone())
        .collect()
}

pub(super) fn visible_agents(state: &ClientShellState) -> Vec<String> {
    state
        .hits
        .agents
        .iter()
        .map(|(_, pane_id)| pane_id.clone())
        .collect()
}

fn focus_target(outcome: &ClientShellInput) -> Option<String> {
    outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. } => match &request.method {
            crate::api::schema::Method::WorkspaceFocus(target) => Some(target.workspace_id.clone()),
            _ => None,
        },
        _ => None,
    })
}

fn enter_navigation(state: &mut ClientShellState) {
    state.handle_input_bytes(&[0x02]);
    state.handle_input_bytes(b"w");
    assert_eq!(state.mode, ClientShellMode::Navigate);
}

#[test]
fn unset_token_leaves_spaces_agents_and_navigation_unchanged() {
    let mut state = contextual_state(None);
    state.compose(106, 30).expect("composed frame");
    assert!(!state.contexts.enabled());
    assert_eq!(visible_workspaces(&state), ["ws_1", "ws_2", "ws_3", "ws_4"]);
    assert_eq!(visible_agents(&state), ["pane_1", "pane_2"]);
    enter_navigation(&mut state);
    let outcome = state.handle_input_bytes(b"3");
    assert_eq!(focus_target(&outcome).as_deref(), Some("ws_3"));
}

#[test]
fn default_tab_shows_only_untagged_workspaces() {
    let mut state = contextual_state(Some("context"));
    state.compose(106, 30).expect("composed frame");
    assert_eq!(state.contexts.active(), &contexts::ActiveContext::Default);
    assert_eq!(visible_workspaces(&state), ["ws_1"]);
    assert_eq!(visible_agents(&state), ["pane_1"]);
    assert_eq!(
        state.contexts.known().collect::<Vec<_>>(),
        ["kund", "privat"]
    );
}

#[test]
fn named_tab_filters_rows_agents_number_keys_and_cycling() {
    let mut state = contextual_state(Some("context"));
    state
        .contexts
        .set_active(contexts::ActiveContext::Named("kund".into()));
    state.compose(106, 30).expect("composed frame");
    assert_eq!(visible_workspaces(&state), ["ws_2", "ws_4"]);
    assert_eq!(visible_agents(&state), ["pane_2"]);

    // Navigate mode starts on a visible workspace even though ws_1 is focused.
    enter_navigation(&mut state);
    assert_eq!(
        state.navigate_workspace_id,
        state.navigation_target(&ClientEndpointId::Local, "ws_2")
    );
    // prefix+digit counts visible rows only: "2" is ws_4, "3" does not exist.
    let outcome = state.handle_input_bytes(b"2");
    assert_eq!(focus_target(&outcome).as_deref(), Some("ws_4"));
    enter_navigation(&mut state);
    let outcome = state.handle_input_bytes(b"3");
    assert!(focus_target(&outcome).is_none());
    assert_eq!(state.mode, ClientShellMode::Navigate);
    state.handle_input_bytes(b"\x1b");

    // next/previous workspace never lands on a hidden workspace.
    for action in [
        crate::input::KeybindAction::NextWorkspace,
        crate::input::KeybindAction::PreviousWorkspace,
    ] {
        let method = state
            .endpoint_method_for_action(action)
            .expect("workspace focus method");
        let crate::api::schema::Method::WorkspaceFocus(target) = method else {
            panic!("expected workspace focus");
        };
        assert!(["ws_2", "ws_4"].contains(&target.workspace_id.as_str()));
    }
    // Agent cycling only sees agents of visible workspaces.
    let method = state
        .endpoint_method_for_action(crate::input::KeybindAction::NextAgent)
        .expect("agent focus method");
    assert!(matches!(
        method,
        crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_2"
    ));
}

#[test]
fn all_tab_shows_every_workspace() {
    let mut state = contextual_state(Some("context"));
    state.contexts.set_active(contexts::ActiveContext::All);
    state.compose(106, 30).expect("composed frame");
    assert_eq!(visible_workspaces(&state), ["ws_1", "ws_2", "ws_3", "ws_4"]);
    assert_eq!(visible_agents(&state), ["pane_1", "pane_2"]);
}

#[test]
fn hidden_worktree_root_does_not_group_visible_children() {
    let mut state = contextual_state(Some("context"));
    let mut projected = contextual_snapshot();
    for (index, linked) in [(0, false), (1, true), (3, true)] {
        projected.workspaces[index].worktree = Some(ClientShellWorktree {
            key: "repo".into(),
            label: "repo".into(),
            is_linked_worktree: linked,
        });
    }
    state.set_snapshot(Box::new(projected));
    state
        .contexts
        .set_active(contexts::ActiveContext::Named("kund".into()));
    state.compose(106, 30).expect("composed frame");
    assert_eq!(visible_workspaces(&state), ["ws_2", "ws_4"]);
    assert!(state.hits.workspaces.iter().all(|hit| !hit.indented));
}

#[test]
fn machines_sidebar_filters_every_endpoint() {
    let mut state = contextual_state(Some("context"));
    let profile =
        crate::client::endpoint::SavedSshEndpoint::new("Build", "dev@build.example", "agents")
            .expect("saved endpoint");
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut projected = contextual_snapshot();
    projected.boot_id = "remote-boot".into();
    projected.workspaces[0].tokens = vec![("context".to_owned(), "privat".to_owned())];
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    state
        .contexts
        .set_active(contexts::ActiveContext::Named("privat".into()));
    state.compose(106, 30).expect("composed frame");
    let rows = state
        .hits
        .workspaces
        .iter()
        .map(|hit| (hit.endpoint_id.clone(), hit.workspace_id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            (ClientEndpointId::Local, "ws_3".to_owned()),
            (remote.clone(), "ws_1".to_owned()),
            (remote.clone(), "ws_3".to_owned()),
        ]
    );
    let agents = state
        .hits
        .endpoint_agents
        .iter()
        .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(agents, [(remote, "pane_1".to_owned())]);
}

/// The sidebar heading row without the trailing divider column.
fn heading_row(state: &mut ClientShellState, cols: u16) -> String {
    let frame = state.compose(cols, 30).expect("composed frame");
    frame_rows(&frame)[0]
        .chars()
        .take(usize::from(state.sidebar_width.saturating_sub(1)))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn tab_hits(state: &ClientShellState) -> Vec<contexts::ContextTab> {
    state
        .hits
        .context_tabs
        .iter()
        .map(|(_, tab)| tab.clone())
        .collect()
}

#[test]
fn tab_row_replaces_the_spaces_heading_and_registers_hits() {
    let mut state = contextual_state(None);
    assert_eq!(heading_row(&mut state, 106), " spaces");
    assert!(state.hits.context_tabs.is_empty());

    let mut state = contextual_state(Some("context"));
    assert_eq!(heading_row(&mut state, 106), " + default kund privat all");
    assert_eq!(
        tab_hits(&state),
        [
            contexts::ContextTab::New,
            contexts::ContextTab::Default,
            contexts::ContextTab::Named("kund".into()),
            contexts::ContextTab::Named("privat".into()),
            contexts::ContextTab::All,
        ]
    );
    let (default_rect, _) = &state.hits.context_tabs[1];
    assert_eq!(
        (default_rect.x, default_rect.y, default_rect.width),
        (3, 0, 7)
    );
    let frame = state.compose(106, 30).expect("composed frame");
    let buffer = frame.to_ratatui_buffer().expect("buffer");
    assert!(buffer[(3, 0)]
        .modifier
        .contains(ratatui::style::Modifier::BOLD));
    assert!(!buffer[(11, 0)]
        .modifier
        .contains(ratatui::style::Modifier::BOLD));

    // Without mouse capture the row is drawn but not clickable.
    state.config.mouse_capture = false;
    assert_eq!(heading_row(&mut state, 106), " + default kund privat all");
    assert!(state.hits.context_tabs.is_empty());
}

#[test]
fn narrow_sidebar_truncates_but_keeps_the_active_tab() {
    let mut state = contextual_state(Some("context"));
    for name in ["arkitekturprojektet", "kundleveransen"] {
        state.contexts.add_known(name);
    }
    state
        .contexts
        .set_active(contexts::ActiveContext::Named("kundleveransen".into()));
    state.sidebar_width = 18;
    let row = heading_row(&mut state, 106);
    assert!(row.contains("kundleveransen"), "{row:?}");
    assert!(row.chars().count() <= 17, "{row:?}");
    let active = state
        .hits
        .context_tabs
        .iter()
        .find(|(_, tab)| tab == &contexts::ContextTab::Named("kundleveransen".into()))
        .map(|(rect, _)| *rect)
        .expect("active tab hit");
    assert!(active.right() <= 18);
    assert!(state
        .hits
        .context_tabs
        .iter()
        .all(|(rect, _)| rect.right() <= 18));
}

#[test]
fn machines_sidebar_draws_the_tab_row_too() {
    let mut state = contextual_state(Some("context"));
    let profile =
        crate::client::endpoint::SavedSshEndpoint::new("Build", "dev@build.example", "agents")
            .expect("saved endpoint");
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut projected = contextual_snapshot();
    projected.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    assert_eq!(heading_row(&mut state, 106), " + default kund privat all");
    assert_eq!(state.hits.context_tabs.len(), 5);
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn tab_rect(state: &ClientShellState, tab: &contexts::ContextTab) -> Rect {
    state
        .hits
        .context_tabs
        .iter()
        .find(|(_, candidate)| candidate == tab)
        .map(|(rect, _)| *rect)
        .expect("tab hit")
}

fn report_targets(outcome: &ClientShellInput) -> Vec<(String, Option<String>)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                crate::api::schema::Method::WorkspaceReportMetadata(params) => Some((
                    params.workspace_id.clone(),
                    params.tokens.get("context").cloned().flatten(),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn clicking_a_tab_switches_the_context_and_persists_it() {
    let path = std::env::temp_dir().join(format!(
        "herdr-context-tabs-click-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut config = Config::default();
    config.ui.sidebar.spaces.context_token = Some("context".into());
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&config).with_preferences_path(path.clone()),
    );
    state.sidebar_width = 32;
    state.sidebar_width_manual = true;
    state.set_snapshot(Box::new(contextual_snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("composed frame");

    let kund = tab_rect(&state, &contexts::ContextTab::Named("kund".into()));
    let outcome = click(&mut state, kund.x, kund.y);
    assert!(outcome.repaint);
    assert!(outcome.actions.is_empty());
    state.compose(106, 30).expect("composed frame");
    assert_eq!(visible_workspaces(&state), ["ws_2", "ws_4"]);

    let reloaded = ClientShellState::new(
        ClientShellConfig::from_config(&config).with_preferences_path(path.clone()),
    );
    assert_eq!(
        reloaded.contexts.active(),
        &contexts::ActiveContext::Named("kund".into())
    );
    assert_eq!(
        reloaded.contexts.known().collect::<Vec<_>>(),
        ["kund", "privat"]
    );
    std::fs::remove_file(path).expect("remove preferences");
}

#[test]
fn plus_tab_prompts_for_a_new_context_and_activates_it() {
    let mut state = contextual_state(Some("context"));
    state.compose(106, 30).expect("composed frame");
    let plus = tab_rect(&state, &contexts::ContextTab::New);
    click(&mut state, plus.x, plus.y);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "new context",
            ..
        }))
    ));
    state.handle_input_bytes(b"ops");
    let outcome = state.handle_input_bytes(b"\r");
    assert!(state.overlay.is_none());
    assert!(outcome.actions.is_empty());
    assert_eq!(
        state.contexts.active(),
        &contexts::ActiveContext::Named("ops".into())
    );
    assert_eq!(
        state.contexts.known().collect::<Vec<_>>(),
        ["kund", "ops", "privat"]
    );
    state.compose(106, 30).expect("composed frame");
    assert!(visible_workspaces(&state).is_empty());

    // Built-in labels are refused and leave the selection alone.
    state.open_new_context_overlay(None);
    state.handle_input_bytes(b"all");
    state.handle_input_bytes(b"\r");
    assert_eq!(
        state.contexts.active(),
        &contexts::ActiveContext::Named("ops".into())
    );
    assert!(state.visible_endpoint_notice.is_some());
}

#[test]
fn context_keys_cycle_through_the_tabs() {
    let mut config = Config::default();
    config.ui.sidebar.spaces.context_token = Some("context".into());
    config.keys.next_context = crate::config::BindingConfig::one("prefix+shift+u");
    config.keys.previous_context = crate::config::BindingConfig::one("prefix+shift+y");
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(contextual_snapshot()));
    state.set_pane_surface(surface());
    let press = |state: &mut ClientShellState, key: &[u8]| {
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(key)
    };
    press(&mut state, b"U");
    assert_eq!(
        state.contexts.active(),
        &contexts::ActiveContext::Named("kund".into())
    );
    press(&mut state, b"U");
    press(&mut state, b"U");
    assert_eq!(state.contexts.active(), &contexts::ActiveContext::All);
    press(&mut state, b"U");
    assert_eq!(state.contexts.active(), &contexts::ActiveContext::Default);
    press(&mut state, b"Y");
    assert_eq!(state.contexts.active(), &contexts::ActiveContext::All);

    // The keys are inert while the feature is off.
    let mut config = Config::default();
    config.keys.next_context = crate::config::BindingConfig::one("prefix+shift+u");
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(contextual_snapshot()));
    state.set_pane_surface(surface());
    press(&mut state, b"U");
    assert_eq!(state.contexts.active(), &contexts::ActiveContext::Default);
}

fn created_workspace(workspace_id: &str) -> crate::api::schema::ResponseResult {
    use crate::api::schema::{PaneInfo, TabInfo, WorkspaceInfo};
    crate::api::schema::ResponseResult::WorkspaceCreated {
        workspace: WorkspaceInfo {
            workspace_id: workspace_id.into(),
            number: 9,
            label: "fresh".into(),
            focused: true,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: format!("{workspace_id}-tab"),
            agent_status: AgentStatus::Idle,
            tokens: Default::default(),
            worktree: None,
        },
        tab: TabInfo {
            tab_id: format!("{workspace_id}-tab"),
            workspace_id: workspace_id.into(),
            number: 1,
            label: "1".into(),
            focused: true,
            pane_count: 1,
            agent_status: AgentStatus::Idle,
        },
        root_pane: PaneInfo {
            pane_id: format!("{workspace_id}-pane"),
            terminal_id: "terminal-9".into(),
            workspace_id: workspace_id.into(),
            tab_id: format!("{workspace_id}-tab"),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Idle,
            state_labels: Default::default(),
            tokens: Default::default(),
            agent_session: None,
            scroll: None,
            revision: 1,
        },
    }
}

#[test]
fn new_workspace_under_a_named_tab_reports_the_context() {
    let mut state = contextual_state(Some("context"));
    state
        .contexts
        .set_active(contexts::ActiveContext::Named("kund".into()));
    state.handle_input_bytes(&[0x02]);
    let outcome = state.handle_input_bytes(b"N");
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("workspace create request");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::WorkspaceCreate(_)
    ));
    assert!(matches!(
        state.pending_requests[&request.id].kind,
        PendingEndpointKind::WorkspaceCreateInContext { ref context } if context == "kund"
    ));

    let (_, actions) =
        state.handle_endpoint_result("boot-1", &request.id, Ok(created_workspace("ws_9")));
    let [ClientShellAction::Endpoint { request, .. }] = &actions[..] else {
        panic!("context report follows the create reply");
    };
    let crate::api::schema::Method::WorkspaceReportMetadata(params) = &request.method else {
        panic!("expected report_metadata");
    };
    assert_eq!(params.workspace_id, "ws_9");
    assert_eq!(params.source, contexts::CONTEXT_METADATA_SOURCE);
    assert_eq!(params.tokens.get("context"), Some(&Some("kund".to_owned())));

    // The next snapshot still lacks the token; the optimistic override keeps ws_9 visible.
    let mut projected = contextual_snapshot();
    let mut fresh = projected.workspaces[0].clone();
    fresh.workspace_id = "ws_9".into();
    fresh.active_tab_id = "ws_9-tab".into();
    fresh.number = 9;
    fresh.label = "fresh".into();
    fresh.focused = true;
    projected.workspaces[0].focused = false;
    projected.workspaces.push(fresh);
    projected.focused_workspace_id = Some("ws_9".into());
    state.set_snapshot(Box::new(projected));
    state.compose(106, 30).expect("composed frame");
    assert_eq!(visible_workspaces(&state), ["ws_2", "ws_4", "ws_9"]);

    // Under the default tab the create stays untagged.
    state.activate_context(
        contexts::ActiveContext::Default,
        &mut ClientShellInput::default(),
    );
    state.handle_input_bytes(&[0x02]);
    let outcome = state.handle_input_bytes(b"N");
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("workspace create request");
    };
    assert!(matches!(
        state.pending_requests[&request.id].kind,
        PendingEndpointKind::Generic
    ));
    let (_, actions) =
        state.handle_endpoint_result("boot-1", &request.id, Ok(created_workspace("ws_10")));
    assert!(report_targets(&ClientShellInput {
        actions,
        ..ClientShellInput::default()
    })
    .is_empty());
}
