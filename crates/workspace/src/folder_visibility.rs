use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use db::kvp::KeyValueStore;
use gpui::{App, BorrowAppContext, Entity, Global, Task, WeakEntity};
use project::Project;
use serde::{Deserialize, Serialize};
use ui::ContextMenu;
use util::ResultExt;

use crate::Workspace;

const STORAGE_KEY: &str = "workspace-hidden-folders";

#[derive(Default, Serialize, Deserialize)]
struct HiddenFolderData(BTreeMap<PathBuf, BTreeSet<PathBuf>>);

#[derive(Default)]
pub struct HiddenFolders {
    data: HiddenFolderData,
    pending_write: Option<Task<()>>,
}

impl Global for HiddenFolders {}

#[derive(Clone, Default)]
pub struct Snapshot {
    entries: Vec<(PathBuf, PathBuf)>,
}

impl Snapshot {
    pub fn is_hidden(&self, path: &Path) -> bool {
        self.entries
            .iter()
            .any(|(_, hidden)| path.starts_with(hidden))
    }
}

impl HiddenFolderData {
    fn snapshot(&self, roots: &[PathBuf]) -> Snapshot {
        Snapshot {
            entries: roots
                .iter()
                .flat_map(|root| {
                    self.0
                        .get(root)
                        .into_iter()
                        .flatten()
                        .map(|path| (root.clone(), path.clone()))
                })
                .collect(),
        }
    }

    fn set(&mut self, root: PathBuf, path: PathBuf, hidden: bool) {
        if path == root || !path.starts_with(&root) {
            return;
        }
        if hidden {
            self.0.entry(root).or_default().insert(path);
        } else if let Some(paths) = self.0.get_mut(&root) {
            paths.remove(&path);
            if paths.is_empty() {
                self.0.remove(&root);
            }
        }
    }
}

pub fn init(cx: &mut App) {
    if cx.has_global::<HiddenFolders>() {
        return;
    }
    let data = KeyValueStore::global(cx)
        .read_kvp(STORAGE_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default();
    cx.set_global(HiddenFolders {
        data,
        pending_write: None,
    });
}

pub fn snapshot(project: &Project, cx: &App) -> Snapshot {
    if !project.is_local() {
        return Snapshot::default();
    }
    let roots: Vec<_> = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    cx.try_global::<HiddenFolders>()
        .map(|state| state.data.snapshot(&roots))
        .unwrap_or_default()
}

pub fn set_hidden(
    root: PathBuf,
    path: PathBuf,
    hidden: bool,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    update(move |data| data.set(root, path, hidden), workspace, cx);
}

fn update(
    change: impl FnOnce(&mut HiddenFolderData),
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    init(cx);
    let (previous_write, serialized) = cx.update_global::<HiddenFolders, _>(|state, _| {
        change(&mut state.data);
        (
            state.pending_write.take(),
            serde_json::to_string(&state.data),
        )
    });
    let database = KeyValueStore::global(cx);
    let task = cx.spawn(async move |cx| {
        // Preserve the order of rapid hide/restore requests while writes are queued.
        if let Some(previous_write) = previous_write {
            previous_write.await;
        }
        let result = match serialized {
            Ok(json) => database.write_kvp(STORAGE_KEY.into(), json).await,
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(format!("Failed to save hidden folders: {error}"), cx);
                })
                .log_err();
        }
    });
    cx.update_global::<HiddenFolders, _>(|state, _| state.pending_write = Some(task));
}

pub fn restore_menu(
    menu: ContextMenu,
    project: &Entity<Project>,
    workspace: WeakEntity<Workspace>,
    cx: &App,
) -> ContextMenu {
    let hidden = snapshot(project.read(cx), cx);
    if hidden.entries.is_empty() {
        return menu;
    }
    menu.separator()
        .submenu("Hidden Folders", move |mut menu, _, _| {
            for (root, path) in &hidden.entries {
                let label = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let root = root.clone();
                let path = path.clone();
                let workspace = workspace.clone();
                menu = menu.entry(format!("Show {label}"), None, move |_, cx| {
                    set_hidden(root.clone(), path.clone(), false, workspace.clone(), cx);
                });
            }
            let roots: BTreeSet<_> = hidden
                .entries
                .iter()
                .map(|(root, _)| root.clone())
                .collect();
            let workspace = workspace.clone();
            menu.separator()
                .entry("Show All Hidden Folders", None, move |_, cx| {
                    update(
                        |data| {
                            for root in &roots {
                                data.0.remove(root);
                            }
                        },
                        workspace.clone(),
                        cx,
                    );
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_folders_are_scoped_persistent_and_reversible() {
        let root = PathBuf::from("/project");
        let hidden = root.join("cache");
        let child = hidden.join("nested");
        let mut data = HiddenFolderData::default();
        data.set(root.clone(), hidden.clone(), true);
        data.set(root.clone(), child.clone(), true);
        let snapshot = data.snapshot(&[root.clone()]);
        assert!(snapshot.is_hidden(&hidden.join("file.rs")));
        assert!(!snapshot.is_hidden(&root.join("cache-other/file.rs")));
        assert!(!data.snapshot(&[hidden.clone()]).is_hidden(&child));
        let json = serde_json::to_string(&data).unwrap();
        let mut restored: HiddenFolderData = serde_json::from_str(&json).unwrap();
        restored.set(root.clone(), hidden.clone(), false);
        let snapshot = restored.snapshot(&[root.clone()]);
        assert!(!snapshot.is_hidden(&hidden.join("file.rs")));
        assert!(snapshot.is_hidden(&child));
        restored.set(root.clone(), child.clone(), false);
        restored.set(root.clone(), root.clone(), true);
        restored.set(root.clone(), PathBuf::from("/other"), true);
        assert!(restored.snapshot(&[root]).entries.is_empty());
    }
}
