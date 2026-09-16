// fork: context tabs
use super::*;
use crate::protocol::ClientShellAgent;

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
