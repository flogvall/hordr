use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ClientRemoteCollapsedGroups {
    pub(super) profile_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
}

// fork: context tabs
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ClientContextSelection {
    Default,
    All,
    Named { name: String },
}

// fork: context tabs
/// Backup of one workspace's context so it can be re-reported after a server restart.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct ClientWorkspaceContext {
    pub(super) endpoint: String,
    pub(super) boot_id: String,
    pub(super) workspace_id: String,
    pub(super) context: String,
}

// fork: context tabs
/// The workspace that was focused most recently while a context was active.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct ClientContextRecent {
    pub(super) context: ClientContextSelection,
    pub(super) endpoint: String,
    pub(super) workspace_id: String,
}

// fork: context tabs
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct ClientContextPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) active: Option<ClientContextSelection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) known: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) workspaces: Vec<ClientWorkspaceContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) recent: Vec<ClientContextRecent>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct ClientChromePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_section_split: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_collapsed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_panel_sort: Option<crate::config::AgentPanelSortConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) remote_collapsed_groups: Vec<ClientRemoteCollapsedGroups>,
    // fork: context tabs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) contexts: Option<ClientContextPreferences>,
}

fn fnv1a(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(super) fn path_for_local_endpoint(socket_path: &Path) -> PathBuf {
    let hash = fnv1a(&socket_path.to_string_lossy());
    crate::config::state_dir()
        .join("client-shell")
        .join(format!("local-{hash:016x}.json"))
}

// fork: context tabs
/// `herdr --remote` forwards through a per-process socket, so its chrome state is keyed
/// by the stable target + session pair instead of the socket path.
pub(super) fn path_for_remote_endpoint(endpoint_key: &str) -> PathBuf {
    let hash = fnv1a(endpoint_key);
    crate::config::state_dir()
        .join("client-shell")
        .join(format!("remote-{hash:016x}.json"))
}

pub(super) fn load(path: &Path) -> Option<ClientChromePreferences> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(super) fn store(path: &Path, preferences: ClientChromePreferences) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create client shell state directory: {error}"))?;
    let content = serde_json::to_vec_pretty(&preferences)
        .map_err(|error| format!("failed to encode client shell state: {error}"))?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path
        .file_name()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?
        .to_os_string();
    temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    let temp_path = parent.join(temp_name);
    std::fs::write(&temp_path, content)
        .map_err(|error| format!("failed to write client shell state: {error}"))?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!("failed to replace client shell state: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_paths_are_stable_and_distinct() {
        let first = path_for_local_endpoint(Path::new("/run/herdr/one.sock"));
        let again = path_for_local_endpoint(Path::new("/run/herdr/one.sock"));
        let second = path_for_local_endpoint(Path::new("/run/herdr/two.sock"));
        assert_eq!(first, again);
        assert_ne!(first, second);
    }

    #[test]
    fn legacy_preferences_default_remote_collapses() {
        let preferences: ClientChromePreferences =
            serde_json::from_str(r#"{"collapsed_groups":["/repo"]}"#)
                .expect("legacy client chrome preferences");

        assert_eq!(preferences.collapsed_groups, ["/repo"]);
        assert!(preferences.remote_collapsed_groups.is_empty());
        assert!(preferences.contexts.is_none());
    }

    // fork: context tabs
    #[test]
    fn remote_endpoint_paths_are_stable_and_separate_from_local() {
        let first = path_for_remote_endpoint("dev@build.example\nagents");
        let again = path_for_remote_endpoint("dev@build.example\nagents");
        let other_session = path_for_remote_endpoint("dev@build.example\ndefault");
        assert_eq!(first, again);
        assert_ne!(first, other_session);
        assert_ne!(
            first,
            path_for_local_endpoint(Path::new("/run/herdr/one.sock"))
        );
    }

    // fork: context tabs
    #[test]
    fn context_preferences_round_trip_and_omit_defaults() {
        let preferences = ClientChromePreferences {
            contexts: Some(ClientContextPreferences {
                active: Some(ClientContextSelection::Named {
                    name: "kund".into(),
                }),
                known: vec!["kund".into(), "privat".into()],
                workspaces: vec![ClientWorkspaceContext {
                    endpoint: "local".into(),
                    boot_id: "boot-1".into(),
                    workspace_id: "ws_1".into(),
                    context: "kund".into(),
                }],
                recent: vec![ClientContextRecent {
                    context: ClientContextSelection::Default,
                    endpoint: "local".into(),
                    workspace_id: "ws_2".into(),
                }],
            }),
            ..ClientChromePreferences::default()
        };
        let json = serde_json::to_string(&preferences).expect("encode");
        assert!(json.contains(r#""kind":"named""#));
        let decoded: ClientChromePreferences = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.contexts, preferences.contexts);
        assert!(!serde_json::to_string(&ClientChromePreferences::default())
            .expect("encode default")
            .contains("contexts"));
    }

    #[test]
    fn concurrent_stores_leave_complete_preferences() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-concurrent-preferences-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let writers = (20..28)
            .map(|width| {
                let path = path.clone();
                std::thread::spawn(move || {
                    store(
                        &path,
                        ClientChromePreferences {
                            sidebar_width: Some(width),
                            ..ClientChromePreferences::default()
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().expect("preference writer").expect("store");
        }
        assert!(load(&path)
            .and_then(|saved| saved.sidebar_width)
            .is_some_and(|width| (20..28).contains(&width)));
        std::fs::remove_file(path).expect("remove preferences");
    }

    #[test]
    fn repeated_store_replaces_existing_preferences() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-preferences-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        store(
            &path,
            ClientChromePreferences {
                sidebar_width: Some(24),
                ..ClientChromePreferences::default()
            },
        )
        .expect("first preference store");
        store(
            &path,
            ClientChromePreferences {
                sidebar_width: Some(32),
                ..ClientChromePreferences::default()
            },
        )
        .expect("replacement preference store");
        assert_eq!(load(&path).and_then(|saved| saved.sidebar_width), Some(32));
        std::fs::remove_file(path).expect("remove preferences");
    }
}
