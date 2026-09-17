// fork: context tabs
//! Context tabs: a client-only grouping of Spaces by one workspace metadata token.
//!
//! The server owns the token values. This module only tracks which contexts the
//! client knows about, which one is active, a short-lived optimistic overlay while
//! a `workspace.report_metadata` request is in flight, and a backup of each
//! workspace's context so it can be re-reported after the server restarts and
//! forgets its (unpersisted) metadata tokens.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::preferences::{
    ClientContextPreferences, ClientContextSelection, ClientWorkspaceContext,
};
use super::ClientEndpointId;
use crate::protocol::{ClientShellSnapshot, ClientShellWorkspace};

/// `source` sent with every `workspace.report_metadata` issued by the tab row.
pub(super) const CONTEXT_METADATA_SOURCE: &str = "herdr-client:context-tabs";
/// Label of the built-in tab that shows every Space.
pub(super) const ALL_CONTEXTS_LABEL: &str = "all";
/// Label of the built-in tab that prompts for a new context.
pub(super) const NEW_CONTEXT_LABEL: &str = "+";
/// Server-side cap for metadata token values.
const MAX_CONTEXT_NAME_LEN: usize = 80;
/// Widest label kept for inactive tabs when the row does not fit.
const COMPACT_TAB_WIDTH: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ActiveContext {
    Default,
    Named(String),
    All,
}

impl ActiveContext {
    pub(super) fn named(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::Default | Self::All => None,
        }
    }
}

/// One clickable cell in the tab row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ContextTab {
    New,
    Default,
    Named(String),
    All,
}

impl ContextTab {
    pub(super) fn label<'a>(&'a self, default_name: &'a str) -> &'a str {
        match self {
            Self::New => NEW_CONTEXT_LABEL,
            Self::Default => default_name,
            Self::Named(name) => name,
            Self::All => ALL_CONTEXTS_LABEL,
        }
    }

    pub(super) fn to_active(&self) -> Option<ActiveContext> {
        match self {
            Self::New => None,
            Self::Default => Some(ActiveContext::Default),
            Self::Named(name) => Some(ActiveContext::Named(name.clone())),
            Self::All => Some(ActiveContext::All),
        }
    }

    pub(super) fn is_active(&self, active: &ActiveContext) -> bool {
        self.to_active().as_ref() == Some(active)
    }

    /// Only user-created contexts can be renamed or removed.
    pub(super) fn named(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            _ => None,
        }
    }
}

/// A `workspace.report_metadata` the client wants to send once the endpoint can take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueuedContextReport {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    pub(super) context: Option<String>,
}

/// Read-only view for filters; carries the endpoint whose snapshot is being inspected.
#[derive(Clone, Copy)]
pub(super) struct ContextView<'a> {
    state: &'a ContextState,
    endpoint_id: &'a ClientEndpointId,
}

impl ContextView<'_> {
    pub(super) fn workspace_visible(&self, workspace: &ClientShellWorkspace) -> bool {
        self.state.workspace_visible(self.endpoint_id, workspace)
    }

    pub(super) fn workspace_id_visible(
        &self,
        snapshot: &ClientShellSnapshot,
        workspace_id: &str,
    ) -> bool {
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .is_some_and(|workspace| self.workspace_visible(workspace))
    }
}

type WorkspaceKey = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Assignment {
    context: String,
    boot_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct ContextState {
    token_key: Option<String>,
    default_name: String,
    active: ActiveContext,
    known: BTreeSet<String>,
    /// Optimistic overrides keyed by (endpoint storage key, workspace id) until the
    /// snapshot confirms the reported value. `None` means "moved to default".
    pending: HashMap<WorkspaceKey, Option<String>>,
    /// Last context the server confirmed per workspace, with the server boot that held it.
    assignments: HashMap<WorkspaceKey, Assignment>,
    queued_reports: Vec<QueuedContextReport>,
    /// Workspaces without the token per endpoint storage key, from the last snapshot.
    /// The default tab is only drawn while one of them exists (or while it is active).
    untagged: HashMap<String, Vec<String>>,
}

impl ContextState {
    pub(super) fn new(
        token_key: Option<String>,
        default_name: &str,
        saved: Option<&ClientContextPreferences>,
    ) -> Self {
        let mut state = Self {
            token_key,
            default_name: default_name.to_owned(),
            active: ActiveContext::Default,
            known: BTreeSet::new(),
            pending: HashMap::new(),
            assignments: HashMap::new(),
            queued_reports: Vec::new(),
            untagged: HashMap::new(),
        };
        let Some(saved) = saved else {
            return state;
        };
        for name in &saved.known {
            state.add_known(name);
        }
        for entry in &saved.workspaces {
            if let Some(context) = Self::normalize_name(&entry.context) {
                state.known.insert(context.clone());
                state.assignments.insert(
                    (entry.endpoint.clone(), entry.workspace_id.clone()),
                    Assignment {
                        context,
                        boot_id: entry.boot_id.clone(),
                    },
                );
            }
        }
        match &saved.active {
            Some(ClientContextSelection::All) => state.active = ActiveContext::All,
            Some(ClientContextSelection::Named { name }) => {
                if let Some(name) = Self::normalize_name(name) {
                    state.set_active(ActiveContext::Named(name));
                }
            }
            Some(ClientContextSelection::Default) | None => {}
        }
        state
    }

    /// Re-applies `[ui.sidebar.spaces]` after a live config reload.
    pub(super) fn apply_config(&mut self, token_key: Option<String>, default_name: &str) {
        self.token_key = token_key;
        if self.default_name != default_name {
            self.default_name = default_name.to_owned();
            self.known.remove(default_name);
            if self.active.named() == Some(default_name) {
                self.active = ActiveContext::Default;
            }
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.token_key.is_some()
    }

    pub(super) fn token_key(&self) -> Option<&str> {
        self.token_key.as_deref()
    }

    pub(super) fn default_name(&self) -> &str {
        &self.default_name
    }

    pub(super) fn active(&self) -> &ActiveContext {
        &self.active
    }

    /// Returns whether the selection changed. Selecting an unknown name registers it.
    pub(super) fn set_active(&mut self, active: ActiveContext) -> bool {
        if let ActiveContext::Named(name) = &active {
            if self.is_reserved_name(name) {
                return false;
            }
            self.known.insert(name.clone());
        }
        if self.active == active {
            return false;
        }
        self.active = active;
        true
    }

    pub(super) fn known(&self) -> impl Iterator<Item = &str> {
        self.known.iter().map(String::as_str)
    }

    /// Whether any workspace currently shows under the default tab, counting moves
    /// that are still in flight.
    pub(super) fn default_has_members(&self) -> bool {
        self.untagged.iter().any(|(endpoint, ids)| {
            ids.iter().any(|id| {
                self.pending
                    .get(&(endpoint.clone(), id.clone()))
                    .is_none_or(|pending| pending.is_none())
            })
        }) || self.pending.values().any(|pending| pending.is_none())
    }

    /// The default tab only takes room while it holds something or is selected; it
    /// comes back on its own as soon as a workspace without the token appears.
    pub(super) fn default_tab_visible(&self) -> bool {
        self.active == ActiveContext::Default || self.default_has_members()
    }

    /// Tab row order: `+`, default (when visible), every known context (sorted), `all`.
    pub(super) fn tabs(&self) -> Vec<ContextTab> {
        let mut tabs = Vec::with_capacity(self.known.len() + 3);
        tabs.push(ContextTab::New);
        if self.default_tab_visible() {
            tabs.push(ContextTab::Default);
        }
        tabs.extend(self.known.iter().cloned().map(ContextTab::Named));
        tabs.push(ContextTab::All);
        tabs
    }

    /// Drops what was learned from an endpoint that left the catalog.
    pub(super) fn forget_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        self.untagged.remove(&endpoint_id.storage_key());
    }

    /// Selectable targets in tab order (everything except `+`), used by next/previous.
    fn selectable(&self) -> Vec<ActiveContext> {
        self.tabs()
            .into_iter()
            .filter_map(|tab| tab.to_active())
            .collect()
    }

    /// The tab `delta` steps away from the active one, wrapping around.
    pub(super) fn next_active(&self, delta: isize) -> ActiveContext {
        let targets = self.selectable();
        let Some(current) = targets.iter().position(|target| target == &self.active) else {
            return ActiveContext::Default;
        };
        let next = (current as isize + delta).rem_euclid(targets.len() as isize) as usize;
        targets[next].clone()
    }

    /// Applies the same normalization the server uses for token values.
    pub(super) fn normalize_name(raw: &str) -> Option<String> {
        let normalized = raw
            .trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(MAX_CONTEXT_NAME_LEN)
            .collect::<String>();
        let trimmed = normalized.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// The default and `all` labels are built-in tabs and cannot double as contexts.
    pub(super) fn is_reserved_name(&self, name: &str) -> bool {
        name == self.default_name || name == ALL_CONTEXTS_LABEL || name == NEW_CONTEXT_LABEL
    }

    pub(super) fn add_known(&mut self, name: &str) -> bool {
        match Self::normalize_name(name) {
            Some(name) if !self.is_reserved_name(&name) => self.known.insert(name),
            _ => false,
        }
    }

    /// Forgets a context; the active tab falls back to default when it pointed at it.
    pub(super) fn remove_known(&mut self, name: &str) -> bool {
        let removed = self.known.remove(name);
        if self.active.named() == Some(name) {
            self.active = ActiveContext::Default;
        }
        for value in self.pending.values_mut() {
            if value.as_deref() == Some(name) {
                *value = None;
            }
        }
        self.assignments
            .retain(|_, assignment| assignment.context != name);
        removed
    }

    pub(super) fn rename_known(&mut self, old: &str, new: &str) -> bool {
        let Some(new) = Self::normalize_name(new) else {
            return false;
        };
        if old == new || self.is_reserved_name(&new) || !self.known.remove(old) {
            return false;
        }
        self.known.insert(new.clone());
        if self.active.named() == Some(old) {
            self.active = ActiveContext::Named(new.clone());
        }
        for value in self.pending.values_mut() {
            if value.as_deref() == Some(old) {
                *value = Some(new.clone());
            }
        }
        for assignment in self.assignments.values_mut() {
            if assignment.context == old {
                assignment.context = new.clone();
            }
        }
        true
    }

    pub(super) fn view<'a>(&'a self, endpoint_id: &'a ClientEndpointId) -> Option<ContextView<'a>> {
        self.enabled().then_some(ContextView {
            state: self,
            endpoint_id,
        })
    }

    fn key(endpoint_id: &ClientEndpointId, workspace_id: &str) -> WorkspaceKey {
        (endpoint_id.storage_key(), workspace_id.to_owned())
    }

    fn token_value<'a>(&self, workspace: &'a ClientShellWorkspace) -> Option<&'a str> {
        let key = self.token_key.as_deref()?;
        workspace
            .tokens
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .filter(|value| *value != self.default_name)
    }

    /// The context a workspace is shown under: the in-flight override wins over the
    /// snapshot token; a token equal to the default name counts as no context.
    pub(super) fn workspace_context<'a>(
        &'a self,
        endpoint_id: &ClientEndpointId,
        workspace: &'a ClientShellWorkspace,
    ) -> Option<&'a str> {
        if let Some(pending) = self
            .pending
            .get(&Self::key(endpoint_id, &workspace.workspace_id))
        {
            return pending.as_deref();
        }
        self.token_value(workspace)
    }

    pub(super) fn workspace_visible(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace: &ClientShellWorkspace,
    ) -> bool {
        if !self.enabled() {
            return true;
        }
        match &self.active {
            ActiveContext::All => true,
            ActiveContext::Default => self.workspace_context(endpoint_id, workspace).is_none(),
            ActiveContext::Named(name) => {
                self.workspace_context(endpoint_id, workspace) == Some(name.as_str())
            }
        }
    }

    /// Workspaces of one endpoint currently shown under `name` (used for rename/remove fan-out).
    pub(super) fn workspaces_in_context(
        &self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
        name: &str,
    ) -> Vec<String> {
        snapshot
            .workspaces
            .iter()
            .filter(|workspace| self.workspace_context(endpoint_id, workspace) == Some(name))
            .map(|workspace| workspace.workspace_id.clone())
            .collect()
    }

    pub(super) fn set_pending(
        &mut self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
        context: Option<String>,
    ) {
        if let Some(name) = &context {
            self.known.insert(name.clone());
        }
        self.pending
            .insert(Self::key(endpoint_id, workspace_id), context);
    }

    pub(super) fn clear_pending(&mut self, endpoint_id: &ClientEndpointId, workspace_id: &str) {
        self.pending.remove(&Self::key(endpoint_id, workspace_id));
    }

    /// Reconciles the client view with a fresh endpoint snapshot. Returns whether the
    /// persisted state changed. Queues a re-report for every workspace that lost its
    /// token to a server restart (the boot id changed while the backup still holds one).
    pub(super) fn observe_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
    ) -> bool {
        if !self.enabled() {
            return false;
        }
        let endpoint = endpoint_id.storage_key();
        let mut changed = false;
        let mut seen = HashSet::new();
        self.untagged.insert(
            endpoint.clone(),
            snapshot
                .workspaces
                .iter()
                .filter(|workspace| self.token_value(workspace).is_none())
                .map(|workspace| workspace.workspace_id.clone())
                .collect(),
        );
        for workspace in &snapshot.workspaces {
            let key = (endpoint.clone(), workspace.workspace_id.clone());
            seen.insert(key.clone());
            match self.token_value(workspace).map(str::to_owned) {
                Some(value) => {
                    changed |= self.known.insert(value.clone());
                    let assignment = Assignment {
                        context: value.clone(),
                        boot_id: snapshot.boot_id.clone(),
                    };
                    if self.assignments.get(&key) != Some(&assignment) {
                        self.assignments.insert(key.clone(), assignment);
                        changed = true;
                    }
                    if self
                        .pending
                        .get(&key)
                        .is_some_and(|pending| pending.as_deref() == Some(value.as_str()))
                    {
                        self.pending.remove(&key);
                    }
                }
                None => {
                    match self.assignments.get(&key).cloned() {
                        Some(saved) if saved.boot_id != snapshot.boot_id => {
                            self.queued_reports.push(QueuedContextReport {
                                endpoint_id: endpoint_id.clone(),
                                workspace_id: workspace.workspace_id.clone(),
                                context: Some(saved.context.clone()),
                            });
                            self.pending
                                .insert(key.clone(), Some(saved.context.clone()));
                            self.assignments.insert(
                                key.clone(),
                                Assignment {
                                    context: saved.context,
                                    boot_id: snapshot.boot_id.clone(),
                                },
                            );
                            changed = true;
                        }
                        // Same server boot: the token was cleared on purpose unless a
                        // move to another context is still in flight.
                        Some(_)
                            if !self
                                .pending
                                .get(&key)
                                .is_some_and(|pending| pending.is_some()) =>
                        {
                            self.assignments.remove(&key);
                            changed = true;
                        }
                        Some(_) | None => {}
                    }
                    if self
                        .pending
                        .get(&key)
                        .is_some_and(|pending| pending.is_none())
                    {
                        self.pending.remove(&key);
                    }
                }
            }
        }
        let before = self.assignments.len();
        self.assignments
            .retain(|key, _| key.0 != endpoint || seen.contains(key));
        changed |= self.assignments.len() != before;
        self.pending
            .retain(|key, _| key.0 != endpoint || seen.contains(key));
        changed
    }

    pub(super) fn take_queued_reports(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) -> Vec<QueuedContextReport> {
        let (mine, rest) = std::mem::take(&mut self.queued_reports)
            .into_iter()
            .partition(|report| &report.endpoint_id == endpoint_id);
        self.queued_reports = rest;
        mine
    }

    pub(super) fn to_preferences(&self) -> Option<ClientContextPreferences> {
        let active = match &self.active {
            ActiveContext::Default => None,
            ActiveContext::All => Some(ClientContextSelection::All),
            ActiveContext::Named(name) => {
                Some(ClientContextSelection::Named { name: name.clone() })
            }
        };
        let mut workspaces = self
            .assignments
            .iter()
            .map(
                |((endpoint, workspace_id), assignment)| ClientWorkspaceContext {
                    endpoint: endpoint.clone(),
                    boot_id: assignment.boot_id.clone(),
                    workspace_id: workspace_id.clone(),
                    context: assignment.context.clone(),
                },
            )
            .collect::<Vec<_>>();
        workspaces.sort_by(|left, right| {
            (&left.endpoint, &left.workspace_id).cmp(&(&right.endpoint, &right.workspace_id))
        });
        let preferences = ClientContextPreferences {
            active,
            known: self.known.iter().cloned().collect(),
            workspaces,
        };
        (preferences != ClientContextPreferences::default()).then_some(preferences)
    }
}

/// One rendered cell of the tab row. `tab` is `None` for overflow markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabCell {
    pub(super) tab: Option<usize>,
    pub(super) text: String,
    pub(super) x: u16,
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn truncate_to(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return if width == 1 {
            "…".to_owned()
        } else {
            String::new()
        };
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Lays out tab labels in `width` cells with one blank cell between tabs. The active
/// tab is never hidden; the others shrink to `COMPACT_TAB_WIDTH`, then drop by distance
/// from the active tab. Dropped sides are marked with `…`.
pub(super) fn layout_tab_row(labels: &[&str], active: usize, width: u16) -> Vec<TabCell> {
    let width = usize::from(width);
    if labels.is_empty() || width == 0 {
        return Vec::new();
    }
    let active = active.min(labels.len() - 1);
    let mut texts = labels
        .iter()
        .map(|label| (*label).to_owned())
        .collect::<Vec<_>>();
    let fits = |texts: &[String], visible: &[usize]| {
        let cells = visible
            .iter()
            .map(|index| display_width(&texts[*index]))
            .sum::<usize>();
        cells + visible.len().saturating_sub(1) <= width
    };
    let mut visible = (0..labels.len()).collect::<Vec<_>>();
    if !fits(&texts, &visible) {
        for (index, text) in texts.iter_mut().enumerate() {
            if index != active && display_width(text) > COMPACT_TAB_WIDTH {
                *text = truncate_to(text, COMPACT_TAB_WIDTH);
            }
        }
    }
    while !fits(&texts, &visible) && visible.len() > 1 {
        let Some(farthest) = visible
            .iter()
            .copied()
            .filter(|index| *index != active)
            .max_by_key(|index| (index.abs_diff(active), *index))
        else {
            break;
        };
        visible.retain(|index| *index != farthest);
    }
    let mut hidden_left = visible.first().is_some_and(|first| *first > 0);
    let mut hidden_right = visible.last().is_some_and(|last| *last + 1 < labels.len());
    // Overflow markers are decoration: drop them before shortening the active label.
    let used = visible
        .iter()
        .map(|index| display_width(&texts[*index]))
        .sum::<usize>()
        + visible.len().saturating_sub(1);
    let room = width.saturating_sub(used);
    if hidden_right && room < 2 * (usize::from(hidden_left) + 1) {
        hidden_right = false;
    }
    if hidden_left && room < 2 {
        hidden_left = false;
    }
    if visible == [active] && display_width(&texts[active]) > width {
        texts[active] = truncate_to(&texts[active], width);
    }
    let mut cells = Vec::new();
    let mut x = 0usize;
    if hidden_left {
        cells.push(TabCell {
            tab: None,
            text: "…".to_owned(),
            x: 0,
        });
        x = 2;
    }
    for index in &visible {
        let text = texts[*index].clone();
        let cell_width = display_width(&text);
        if x + cell_width > width {
            break;
        }
        cells.push(TabCell {
            tab: Some(*index),
            text,
            x: x as u16,
        });
        x += cell_width + 1;
    }
    if hidden_right && x <= width.saturating_sub(1) {
        cells.push(TabCell {
            tab: None,
            text: "…".to_owned(),
            x: x as u16,
        });
    }
    cells
}

impl super::ClientShellState {
    /// Switches the active tab; the list scrolls back to the top so the selection shows.
    pub(super) fn activate_context(
        &mut self,
        active: ActiveContext,
        outcome: &mut super::ClientShellInput,
    ) {
        if !self.contexts.enabled() {
            return;
        }
        if self.contexts.set_active(active) {
            self.workspace_scroll = 0;
            self.agent_scroll = 0;
            self.reveal_focused_workspace = true;
            self.persist_chrome_preferences(outcome);
        }
        if self.mode == super::ClientShellMode::Navigate {
            self.navigate_workspace_id = self.visible_navigation_target();
            self.reveal_navigation_workspace = true;
        }
        outcome.repaint = true;
    }

    pub(super) fn cycle_context(&mut self, delta: isize, outcome: &mut super::ClientShellInput) {
        if !self.contexts.enabled() {
            return;
        }
        let next = self.contexts.next_active(delta);
        self.activate_context(next, outcome);
    }

    pub(super) fn handle_context_tab_click(
        &mut self,
        tab: ContextTab,
        outcome: &mut super::ClientShellInput,
    ) {
        match tab.to_active() {
            Some(active) => self.activate_context(active, outcome),
            None => {
                self.open_new_context_overlay(None);
                outcome.repaint = true;
            }
        }
    }

    /// Reuses the inline rename prompt to ask for a context name.
    pub(super) fn open_new_context_overlay(&mut self, workspace_id: Option<String>) {
        self.overlay = Some(super::ClientShellOverlay::Rename(
            super::ClientRenameOverlay {
                title: "new context",
                input: super::TextEditor::new("", true),
                target: super::ClientRenameTarget::NewContext { workspace_id },
            },
        ));
    }

    fn accept_context_name(&mut self, raw: &str) -> Option<String> {
        let name = ContextState::normalize_name(raw)?;
        if self.contexts.is_reserved_name(&name) {
            self.receive_endpoint_unavailable(format!(
                "\"{name}\" is a built-in tab; choose another context name"
            ));
            return None;
        }
        Some(name)
    }

    /// Registers a context typed in the prompt. With a workspace it moves that
    /// workspace into the new context; without one it activates the new tab.
    pub(super) fn create_context(
        &mut self,
        raw_name: &str,
        workspace_id: Option<String>,
        outcome: &mut super::ClientShellInput,
    ) {
        outcome.repaint = true;
        let Some(name) = self.accept_context_name(raw_name) else {
            return;
        };
        self.contexts.add_known(&name);
        match workspace_id {
            Some(workspace_id) => {
                self.push_context_report(workspace_id, Some(name), outcome);
                self.persist_chrome_preferences(outcome);
            }
            None => self.activate_context(ActiveContext::Named(name), outcome),
        }
    }

    /// Renames a context and rewrites the token on every workspace that carries it.
    pub(super) fn rename_context(
        &mut self,
        old: &str,
        raw_new: &str,
        outcome: &mut super::ClientShellInput,
    ) {
        outcome.repaint = true;
        let Some(new) = self.accept_context_name(raw_new) else {
            return;
        };
        if new == old {
            return;
        }
        let endpoint_id = self.active_endpoint_id.clone();
        let members = self
            .snapshot
            .as_deref()
            .map(|snapshot| {
                self.contexts
                    .workspaces_in_context(&endpoint_id, snapshot, old)
            })
            .unwrap_or_default();
        if !self.contexts.rename_known(old, &new) {
            return;
        }
        for workspace_id in members {
            self.push_context_report(workspace_id, Some(new.clone()), outcome);
        }
        self.persist_chrome_preferences(outcome);
    }

    /// Opens the context list for the navigate highlight or the focused workspace,
    /// anchored at its sidebar row.
    pub(super) fn open_context_list_for_selected_workspace(&mut self) {
        if !self.contexts.enabled() {
            return;
        }
        let Some(workspace_id) = self.workspace_action_id() else {
            return;
        };
        let (x, y) = self
            .hits
            .workspaces
            .iter()
            .find(|hit| {
                hit.endpoint_id == self.active_endpoint_id && hit.workspace_id == workspace_id
            })
            .map(|hit| (hit.rect.x.saturating_add(2), hit.rect.y.saturating_add(1)))
            .unwrap_or((1, 1));
        self.open_workspace_context_list_menu(workspace_id, x, y);
    }

    /// Reuses the close-workspace confirmation for removing a context.
    pub(super) fn open_confirm_remove_context_overlay(&mut self, name: String) {
        let endpoint_id = self.active_endpoint_id.clone();
        let count = self
            .snapshot
            .as_deref()
            .map(|snapshot| {
                self.contexts
                    .workspaces_in_context(&endpoint_id, snapshot, &name)
                    .len()
            })
            .unwrap_or(0);
        let spaces = if count == 1 {
            "1 space".to_owned()
        } else {
            format!("{count} spaces")
        };
        self.overlay = Some(super::ClientShellOverlay::ConfirmClose(
            super::ClientConfirmCloseOverlay {
                workspace_id: String::new(),
                title: "Remove context?".to_owned(),
                detail: format!("{name} — {spaces} move to {}", self.contexts.default_name()),
                remove_context: Some(name),
            },
        ));
    }

    /// Removes a context: its workspaces lose the token (and land in default) and the
    /// tab disappears; default becomes active when the removed tab was.
    pub(super) fn remove_context(&mut self, name: &str, outcome: &mut super::ClientShellInput) {
        outcome.repaint = true;
        let endpoint_id = self.active_endpoint_id.clone();
        let members = self
            .snapshot
            .as_deref()
            .map(|snapshot| {
                self.contexts
                    .workspaces_in_context(&endpoint_id, snapshot, name)
            })
            .unwrap_or_default();
        if !self.contexts.remove_known(name) {
            return;
        }
        for workspace_id in members {
            self.push_context_report(workspace_id, None, outcome);
        }
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        if self.mode == super::ClientShellMode::Navigate {
            self.navigate_workspace_id = self.visible_navigation_target();
            self.reveal_navigation_workspace = true;
        }
        self.persist_chrome_preferences(outcome);
    }

    /// Sends `workspace.create`; under a named tab the reply is tagged so the new
    /// workspace joins that context as soon as its id is known.
    pub(super) fn push_workspace_create(
        &mut self,
        params: crate::api::schema::WorkspaceCreateParams,
        outcome: &mut super::ClientShellInput,
    ) {
        let kind = match self.contexts.active().named() {
            Some(context) if self.contexts.enabled() => {
                super::PendingEndpointKind::WorkspaceCreateInContext {
                    context: context.to_owned(),
                }
            }
            _ => super::PendingEndpointKind::Generic,
        };
        self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorkspaceCreate(params),
            kind,
            outcome,
        );
    }
}

/// Draws the context bar across the top of the client and registers one hit rect per tab.
pub(super) fn render_tab_row(
    buffer: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    state: &ContextState,
    palette: &crate::app::state::Palette,
    register_hits: bool,
    hits: &mut Vec<(ratatui::layout::Rect, ContextTab)>,
) {
    use ratatui::style::{Modifier, Style};

    if area.height == 0 || area.width < 2 {
        return;
    }
    buffer.set_style(area, Style::default().bg(palette.panel_bg));
    let tabs = state.tabs();
    let default_name = state.default_name();
    let labels = tabs
        .iter()
        .map(|tab| tab.label(default_name))
        .collect::<Vec<_>>();
    let active = tabs
        .iter()
        .position(|tab| tab.is_active(state.active()))
        .unwrap_or(1);
    // Keep the heading's one-cell left margin.
    let row_x = area.x.saturating_add(1);
    let row_width = area.width.saturating_sub(1);
    for cell in layout_tab_row(&labels, active, row_width) {
        let x = row_x.saturating_add(cell.x);
        let width = super::render::display_width(&cell.text);
        let style = match cell.tab {
            Some(index) if index == active => Style::default()
                .fg(palette.accent)
                .bg(palette.panel_bg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            Some(_) => Style::default().fg(palette.overlay0).bg(palette.panel_bg),
            None => Style::default()
                .fg(palette.overlay0)
                .bg(palette.panel_bg)
                .add_modifier(Modifier::DIM),
        };
        super::render::put_text(buffer, x, area.y, width, &cell.text, style);
        if register_hits {
            if let Some(tab) = cell.tab.and_then(|index| tabs.get(index)) {
                hits.push((ratatui::layout::Rect::new(x, area.y, width, 1), tab.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;

    fn workspace(id: &str, context: Option<&str>) -> ClientShellWorkspace {
        ClientShellWorkspace {
            workspace_id: id.into(),
            active_tab_id: format!("{id}-tab"),
            new_workspace_cwd: "/repo".into(),
            number: 1,
            label: id.into(),
            custom_label: false,
            branch: None,
            git_ahead_behind: None,
            tokens: context
                .map(|context| vec![("context".to_owned(), context.to_owned())])
                .unwrap_or_default(),
            worktree: None,
            focused: false,
            agent_status: AgentStatus::Idle,
        }
    }

    fn snapshot(boot_id: &str, workspaces: Vec<ClientShellWorkspace>) -> ClientShellSnapshot {
        let mut snapshot = super::super::tests::snapshot();
        snapshot.boot_id = boot_id.into();
        snapshot.workspaces = workspaces;
        snapshot
    }

    fn enabled() -> ContextState {
        ContextState::new(Some("context".into()), "default", None)
    }

    fn visible_ids(state: &ContextState, snapshot: &ClientShellSnapshot) -> Vec<String> {
        snapshot
            .workspaces
            .iter()
            .filter(|workspace| state.workspace_visible(&ClientEndpointId::Local, workspace))
            .map(|workspace| workspace.workspace_id.clone())
            .collect()
    }

    #[test]
    fn disabled_state_shows_everything_and_tracks_nothing() {
        let mut state = ContextState::new(None, "default", None);
        let snapshot = snapshot(
            "boot-1",
            vec![workspace("a", Some("kund")), workspace("b", None)],
        );
        assert!(!state.enabled());
        assert!(state.view(&ClientEndpointId::Local).is_none());
        assert!(!state.observe_snapshot(&ClientEndpointId::Local, &snapshot));
        assert_eq!(state.known().count(), 0);
        assert_eq!(visible_ids(&state, &snapshot), ["a", "b"]);
        assert!(state.to_preferences().is_none());
    }

    #[test]
    fn workspaces_without_token_belong_to_default() {
        let mut state = enabled();
        let snapshot = snapshot(
            "boot-1",
            vec![
                workspace("a", Some("kund")),
                workspace("b", None),
                workspace("c", Some("default")),
            ],
        );
        state.observe_snapshot(&ClientEndpointId::Local, &snapshot);
        assert_eq!(state.known().collect::<Vec<_>>(), ["kund"]);
        assert_eq!(visible_ids(&state, &snapshot), ["b", "c"]);
        state.set_active(ActiveContext::Named("kund".into()));
        assert_eq!(visible_ids(&state, &snapshot), ["a"]);
        state.set_active(ActiveContext::All);
        assert_eq!(visible_ids(&state, &snapshot), ["a", "b", "c"]);
    }

    #[test]
    fn tabs_list_default_known_and_all_in_order() {
        let mut state = enabled();
        state.add_known("privat");
        state.add_known("kund");
        state.add_known(" ");
        state.add_known("all");
        state.add_known("default");
        assert_eq!(
            state.tabs(),
            [
                ContextTab::New,
                ContextTab::Default,
                ContextTab::Named("kund".into()),
                ContextTab::Named("privat".into()),
                ContextTab::All,
            ]
        );
        let cycle = |state: &mut ContextState, delta: isize| {
            let next = state.next_active(delta);
            assert!(state.set_active(next));
        };
        cycle(&mut state, 1);
        assert_eq!(state.active(), &ActiveContext::Named("kund".into()));
        // Nothing is untagged, so the default tab drops out once it is not selected.
        assert!(!state.tabs().contains(&ContextTab::Default));
        cycle(&mut state, -1);
        assert_eq!(state.active(), &ActiveContext::All);
        cycle(&mut state, 1);
        assert_eq!(state.active(), &ActiveContext::Named("kund".into()));
        assert!(state.set_active(ActiveContext::Default));
        assert!(state.tabs().contains(&ContextTab::Default));
        // An untagged workspace keeps the tab around regardless of the selection.
        state.observe_snapshot(
            &ClientEndpointId::Local,
            &snapshot("boot-1", vec![workspace("a", None)]),
        );
        assert!(state.set_active(ActiveContext::All));
        assert!(state.default_has_members());
        assert!(state.tabs().contains(&ContextTab::Default));
    }

    #[test]
    fn pending_moves_override_the_snapshot_until_confirmed() {
        let mut state = enabled();
        let local = ClientEndpointId::Local;
        let before = snapshot("boot-1", vec![workspace("a", Some("kund"))]);
        state.observe_snapshot(&local, &before);
        state.set_active(ActiveContext::Named("kund".into()));
        state.set_pending(&local, "a", Some("privat".into()));
        assert!(visible_ids(&state, &before).is_empty());
        state.set_active(ActiveContext::Named("privat".into()));
        assert_eq!(visible_ids(&state, &before), ["a"]);
        // A stale snapshot keeps the override; the confirming one clears it.
        state.observe_snapshot(&local, &before);
        assert_eq!(visible_ids(&state, &before), ["a"]);
        let after = snapshot("boot-1", vec![workspace("a", Some("privat"))]);
        state.observe_snapshot(&local, &after);
        assert_eq!(visible_ids(&state, &after), ["a"]);
        assert!(state.pending.is_empty());
        // Moving to default clears the token and confirms on the empty snapshot.
        state.set_pending(&local, "a", None);
        state.set_active(ActiveContext::Default);
        assert_eq!(visible_ids(&state, &after), ["a"]);
        let cleared = snapshot("boot-1", vec![workspace("a", None)]);
        assert!(state.observe_snapshot(&local, &cleared));
        assert!(state.pending.is_empty());
        assert!(state.assignments.is_empty());
    }

    #[test]
    fn server_restart_requeues_the_backup_but_a_plain_clear_does_not() {
        let mut state = enabled();
        let local = ClientEndpointId::Local;
        state.observe_snapshot(
            &local,
            &snapshot("boot-1", vec![workspace("a", Some("kund"))]),
        );
        // Same boot, token gone: someone cleared it on purpose.
        state.observe_snapshot(&local, &snapshot("boot-1", vec![workspace("a", None)]));
        assert!(state.take_queued_reports(&local).is_empty());
        assert!(state.assignments.is_empty());

        state.observe_snapshot(
            &local,
            &snapshot("boot-1", vec![workspace("a", Some("kund"))]),
        );
        let restarted = snapshot("boot-2", vec![workspace("a", None)]);
        assert!(state.observe_snapshot(&local, &restarted));
        assert_eq!(
            state.take_queued_reports(&local),
            [QueuedContextReport {
                endpoint_id: local.clone(),
                workspace_id: "a".into(),
                context: Some("kund".into()),
            }]
        );
        assert!(state.take_queued_reports(&local).is_empty());
        state.set_active(ActiveContext::Named("kund".into()));
        assert_eq!(visible_ids(&state, &restarted), ["a"]);
        // A second snapshot from the same boot must not queue again.
        assert!(!state.observe_snapshot(&local, &restarted));
        assert!(state.take_queued_reports(&local).is_empty());
    }

    #[test]
    fn closed_workspaces_drop_their_backup() {
        let mut state = enabled();
        let local = ClientEndpointId::Local;
        state.observe_snapshot(
            &local,
            &snapshot(
                "boot-1",
                vec![workspace("a", Some("kund")), workspace("b", Some("kund"))],
            ),
        );
        assert!(state.observe_snapshot(
            &local,
            &snapshot("boot-1", vec![workspace("b", Some("kund"))])
        ));
        assert_eq!(state.assignments.len(), 1);
        let ssh = ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate());
        state.observe_snapshot(
            &ssh,
            &snapshot("boot-9", vec![workspace("z", Some("privat"))]),
        );
        // Another endpoint's snapshot never touches local assignments.
        state.observe_snapshot(&ssh, &snapshot("boot-9", vec![]));
        assert_eq!(state.assignments.len(), 1);
    }

    #[test]
    fn rename_and_remove_follow_active_pending_and_backups() {
        let mut state = enabled();
        let local = ClientEndpointId::Local;
        state.observe_snapshot(
            &local,
            &snapshot("boot-1", vec![workspace("a", Some("kund"))]),
        );
        state.set_active(ActiveContext::Named("kund".into()));
        state.set_pending(&local, "b", Some("kund".into()));
        assert!(state.rename_known("kund", "  kunden  "));
        assert!(!state.rename_known("kunden", "all"));
        assert!(!state.rename_known("missing", "x"));
        assert_eq!(state.active(), &ActiveContext::Named("kunden".into()));
        assert_eq!(state.known().collect::<Vec<_>>(), ["kunden"]);
        assert_eq!(
            state.pending.values().next(),
            Some(&Some("kunden".to_owned()))
        );
        assert_eq!(
            state
                .assignments
                .values()
                .next()
                .map(|a| a.context.as_str()),
            Some("kunden")
        );

        assert!(state.remove_known("kunden"));
        assert_eq!(state.active(), &ActiveContext::Default);
        assert_eq!(state.known().count(), 0);
        assert!(state.assignments.is_empty());
        assert_eq!(state.pending.values().next(), Some(&None));
    }

    #[test]
    fn preferences_round_trip() {
        let mut state = enabled();
        let local = ClientEndpointId::Local;
        state.observe_snapshot(
            &local,
            &snapshot("boot-1", vec![workspace("a", Some("kund"))]),
        );
        state.add_known("privat");
        state.set_active(ActiveContext::Named("privat".into()));
        let saved = state.to_preferences().expect("non-empty preferences");
        let json = serde_json::to_string(&saved).expect("encode");
        let decoded: ClientContextPreferences = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, saved);
        let restored = ContextState::new(Some("context".into()), "default", Some(&decoded));
        assert_eq!(restored.active(), &ActiveContext::Named("privat".into()));
        assert_eq!(restored.known().collect::<Vec<_>>(), ["kund", "privat"]);
        assert_eq!(restored.assignments, state.assignments);
        assert_eq!(restored.to_preferences(), Some(saved));
        // Legacy files without the section still load.
        let legacy: super::super::preferences::ClientChromePreferences =
            serde_json::from_str(r#"{"collapsed_groups":["/repo"]}"#).expect("legacy");
        assert!(legacy.contexts.is_none());
    }

    #[test]
    fn reserved_names_are_rejected_and_config_reload_reapplies_default() {
        let mut state = enabled();
        assert!(!state.set_active(ActiveContext::Named("default".into())));
        assert!(!state.set_active(ActiveContext::Named("all".into())));
        assert!(state.set_active(ActiveContext::Named("work".into())));
        state.apply_config(Some("ctx".into()), "work");
        assert_eq!(state.active(), &ActiveContext::Default);
        assert_eq!(state.known().count(), 0);
        assert_eq!(state.token_key(), Some("ctx"));
        state.apply_config(None, "work");
        assert!(!state.enabled());
    }

    fn layout(labels: &[&str], active: usize, width: u16) -> Vec<(Option<usize>, String, u16)> {
        layout_tab_row(labels, active, width)
            .into_iter()
            .map(|cell| (cell.tab, cell.text, cell.x))
            .collect()
    }

    #[test]
    fn tab_row_fits_wide_and_truncates_narrow() {
        let labels = ["+", "default", "kund", "privat", "all"];
        assert_eq!(
            layout(&labels, 1, 40),
            [
                (Some(0), "+".to_owned(), 0),
                (Some(1), "default".to_owned(), 2),
                (Some(2), "kund".to_owned(), 10),
                (Some(3), "privat".to_owned(), 15),
                (Some(4), "all".to_owned(), 22),
            ]
        );
        // 17 cells: inactive labels shrink first, then the farthest tabs drop.
        let narrow = layout(&labels, 3, 17);
        assert!(narrow
            .iter()
            .any(|(tab, text, _)| *tab == Some(3) && text == "privat"));
        assert!(narrow
            .iter()
            .all(|(_, text, x)| usize::from(*x) + display_width(text) <= 17));
        assert!(narrow.iter().any(|(tab, _, _)| tab.is_none()));
        // The active tab survives even when nothing else fits.
        let tiny = layout(&labels, 2, 4);
        assert!(tiny.iter().any(|(tab, _, _)| *tab == Some(2)));
        assert!(tiny
            .iter()
            .all(|(_, text, x)| usize::from(*x) + display_width(text) <= 4));
        assert!(layout(&labels, 0, 0).is_empty());
    }
}
