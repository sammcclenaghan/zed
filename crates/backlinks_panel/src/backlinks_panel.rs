use anyhow::Result;
use editor::Editor;
use fs::Fs;
use gpui::{
    actions, div, uniform_list, App, AppContext as _, AsyncApp, AsyncWindowContext, Context,
    Entity, EventEmitter, FocusHandle, Focusable, ListHorizontalSizingBehavior, ListSizingBehavior,
    Pixels, Render, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
};

use project::{Project, ProjectItem, ProjectPath, WorktreeId};
use regex::Regex;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{path::PathBuf, sync::Arc};
use theme::ThemeSettings;
use ui::{prelude::*, Icon, IconName, Label, ListItem, ListItemSpacing};

use panel::PanelHeader;
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    Workspace,
};

actions!(backlinks_panel, [ToggleFocus]);

const BACKLINKS_PANEL_KEY: &str = "BacklinksPanel";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<BacklinksPanel>(window, cx);
        });
    })
    .detach();
}

/// Represents a backlink entry - a file that links to the current file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklinkEntry {
    /// The project path of the file containing the backlink
    pub path: ProjectPath,
    /// The absolute path of the file for display purposes
    pub abs_path: PathBuf,
    /// The display name (filename) of the linking file
    pub display_name: String,
    /// The worktree ID this file belongs to
    pub worktree_id: WorktreeId,
    /// Context around the link (the line containing the link)
    pub context: String,
    /// Line number where the link appears (0-indexed)
    pub line_number: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedBacklinksPanel {
    width: Option<Pixels>,
}

pub struct BacklinksPanel {
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    entries: Vec<BacklinkEntry>,
    scroll_handle: UniformListScrollHandle,
    workspace: WeakEntity<Workspace>,
    width: Option<Pixels>,
    pending_serialization: Task<()>,
    current_file_path: Option<ProjectPath>,
    update_task: Option<Task<()>>,
    _settings_subscription: Subscription,
}

impl BacklinksPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let fs = workspace.update(&mut cx, |workspace, _| workspace.app_state().fs.clone())?;
        let project = workspace.update(&mut cx, |workspace, _| workspace.project().clone())?;

        workspace.update_in(&mut cx, |workspace, _window, cx| {
            cx.new(|cx| {
                let focus_handle = cx.focus_handle();
                let settings_subscription = cx.observe_global::<ThemeSettings>(move |_, cx| {
                    cx.notify();
                });

                Self {
                    project: project.clone(),
                    fs,
                    focus_handle,
                    entries: Vec::new(),
                    scroll_handle: UniformListScrollHandle::new(),
                    workspace: workspace.weak_handle(),
                    width: None,
                    pending_serialization: Task::ready(()),
                    current_file_path: None,
                    update_task: None,
                    _settings_subscription: settings_subscription,
                }
            })
        })
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let width = self.width;
        self.pending_serialization = cx.background_spawn(async move {
            if let Some(_width) = width {
                // TODO: Store in a proper database once available
                // Could save to preferences when database is available
            }
        });
    }

    /// Update backlinks for the currently active file
    fn update_backlinks(&mut self, file_path: Option<ProjectPath>, cx: &mut Context<Self>) {
        if self.current_file_path == file_path {
            return; // No change needed
        }

        self.current_file_path = file_path.clone();

        if let Some(file_path) = file_path {
            let project = self.project.clone();
            let fs = self.fs.clone();

            let task = cx.spawn(async move |this, mut cx| {
                let backlinks = Self::find_backlinks(project, fs, file_path, &mut cx).await;

                this.update(cx, |this, cx| {
                    match backlinks {
                        Ok(entries) => {
                            this.entries = entries;
                        }
                        Err(_e) => {
                            this.entries.clear();
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
            self.update_task = Some(task);
        } else {
            self.entries.clear();
            cx.notify();
        }
    }

    /// Find all files that link to the given file path
    async fn find_backlinks(
        project: Entity<Project>,
        fs: Arc<dyn Fs>,
        target_path: ProjectPath,
        cx: &mut AsyncApp,
    ) -> Result<Vec<BacklinkEntry>> {
        let target_file_name = target_path
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // If we don't have a valid file name, return empty results
        if target_file_name.is_empty() {
            return Ok(Vec::new());
        }

        let target_abs_path = project
            .read_with(cx, |project, cx| {
                project
                    .worktree_store()
                    .read(cx)
                    .absolutize(&target_path, cx)
            })?
            .unwrap_or_else(|| target_path.path.to_path_buf());

        // Get all markdown files in the project
        let markdown_files = project.read_with(cx, |project, cx| {
            let mut files = Vec::new();
            for worktree_handle in project.worktree_store().read(cx).visible_worktrees(cx) {
                let worktree = worktree_handle.read(cx);
                let worktree_id = worktree.id();
                let worktree_root = worktree.abs_path();

                for entry in worktree.entries(false, 0) {
                    if entry.is_file() {
                        if let Some(extension) = entry.path.extension() {
                            if extension == "md" {
                                let abs_path = worktree_root.join(&entry.path);
                                if abs_path != target_abs_path {
                                    // Don't include the target file itself
                                    files.push((
                                        ProjectPath {
                                            worktree_id,
                                            path: entry.path.clone(),
                                        },
                                        abs_path,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            files
        })?;

        let mut backlinks = Vec::new();

        // Create regex patterns for finding links
        // Pattern 1: [[Note Name]] (wiki-style links)
        let wiki_link_pattern =
            Regex::new(&format!(r"\[\[{}\]\]", regex::escape(&target_file_name)))?;

        // Pattern 2: [Text](filename.md) (markdown links)
        let md_link_pattern =
            if let Some(file_name) = target_path.path.file_name().and_then(|f| f.to_str()) {
                Regex::new(&format!(
                    r"\[([^\]]+)\]\([^)]*{}[^)]*\)",
                    regex::escape(file_name)
                ))?
            } else {
                // If we can't get a valid file name, create a pattern that will never match
                Regex::new(r"(?!.*)")?
            };

        // Scan each markdown file for backlinks
        for (project_path, abs_path) in markdown_files {
            if let Ok(content) = fs.load(&abs_path).await {
                let content_str = content.to_string();
                let lines: Vec<&str> = content_str.lines().collect();

                for (line_number, line) in lines.iter().enumerate() {
                    let has_wiki_link = wiki_link_pattern.is_match(line);
                    let has_md_link = md_link_pattern.is_match(line);

                    if has_wiki_link || has_md_link {
                        let display_name = project_path
                            .path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        backlinks.push(BacklinkEntry {
                            path: project_path.clone(),
                            abs_path: abs_path.clone(),
                            display_name,
                            worktree_id: project_path.worktree_id,
                            context: line.trim().to_string(),
                            line_number,
                        });
                    }
                }
            }
        }

        Ok(backlinks)
    }

    /// Open a backlink entry in the editor
    fn open_backlink(
        &mut self,
        entry: &BacklinkEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_path(entry.path.clone(), None, true, window, cx)
                    .detach();
            });
        }
    }

    fn render_backlink_entry(
        &self,
        ix: usize,
        entry: &BacklinkEntry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entry = entry.clone();
        ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::File)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new(entry.display_name.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(entry.context.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .on_click({
                let entry = entry.clone();
                cx.listener(move |this, _, window, cx| {
                    this.open_backlink(&entry, window, cx);
                })
            })
    }
}

impl EventEmitter<PanelEvent> for BacklinksPanel {}

impl Focusable for BacklinksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PanelHeader for BacklinksPanel {}

impl Panel for BacklinksPanel {
    fn persistent_name() -> &'static str {
        "BacklinksPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // For now, we'll keep the position fixed
        // In the future, we could add settings to customize this
    }

    fn size(&self, _window: &Window, cx: &App) -> Pixels {
        self.width.unwrap_or_else(|| {
            let font_size = ThemeSettings::get_global(cx).ui_font_size(cx);
            font_size * 15.0
        })
    }

    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ArrowLeft)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Backlinks Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for BacklinksPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Check if the current file has changed
        let current_active_file = self.workspace.upgrade().and_then(|workspace| {
            workspace.read(cx).active_item(cx).and_then(|item| {
                item.act_as::<Editor>(cx).and_then(|editor| {
                    let buffer = editor.read(cx).buffer();
                    let buffer = buffer.read(cx);
                    buffer
                        .as_singleton()
                        .and_then(|buffer| buffer.read(cx).project_path(cx))
                })
            })
        });

        self.update_backlinks(current_active_file, cx);

        v_flex()
            .key_context(BACKLINKS_PANEL_KEY)
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                self.panel_header_container(window, cx).child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::ArrowLeft).size(IconSize::Small))
                        .child(Label::new("Backlinks").size(LabelSize::Default)),
                ),
            )
            .child(div().flex_1().min_h_0().child(if self.entries.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new("No backlinks found")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                uniform_list("backlinks-list", self.entries.len(), {
                    cx.processor(|this, range, window, cx| {
                        let mut items = Vec::new();
                        for ix in range {
                            if let Some(entry) = this.entries.get(ix) {
                                items.push(this.render_backlink_entry(ix, entry, window, cx));
                            }
                        }
                        items
                    })
                })
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .track_scroll(self.scroll_handle.clone())
                .into_any_element()
            }))
    }
}
