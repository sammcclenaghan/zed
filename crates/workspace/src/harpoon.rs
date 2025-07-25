use anyhow::Result;
use collections::HashMap;
use gpui::{
    App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global,
    WeakEntity, Window, actions,
};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsSources};
use ui::prelude::*;

// Forward declaration - we can't import these since we're in the workspace crate itself
use crate::{ModalView, Workspace};

actions!(
    harpoon,
    [
        Mark, Jump1, Jump2, Jump3, Jump4, Jump5, Jump6, Jump7, Jump8, Jump9, ShowPicker, Clear
    ]
);

const MAX_HARPOON_SLOTS: usize = 9;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HarpoonSettings {
    /// Maximum number of files that can be marked
    /// Default: 9
    pub max_slots: Option<usize>,
    /// Whether to persist marks across sessions
    /// Default: true
    pub persist_marks: Option<bool>,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct HarpoonSettingsContent {
    /// Maximum number of files that can be marked
    /// Default: 9
    pub max_slots: Option<usize>,
    /// Whether to persist marks across sessions
    /// Default: true
    pub persist_marks: Option<bool>,
}

impl Settings for HarpoonSettings {
    const KEY: Option<&'static str> = Some("harpoon");

    type FileContent = HarpoonSettingsContent;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> Result<Self> {
        sources.json_merge()
    }

    fn import_from_vscode(_: &settings::VsCodeSettings, _: &mut Self::FileContent) {
        // Harpoon doesn't have VS Code equivalent, so no import needed
    }
}

impl Default for HarpoonSettings {
    fn default() -> Self {
        Self {
            max_slots: Some(MAX_HARPOON_SLOTS),
            persist_marks: Some(true),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HarpoonMark {
    pub project_path: ProjectPath,
    pub display_name: String,
}

pub struct HarpoonStore {
    project: WeakEntity<Project>,
    marks: Vec<Option<HarpoonMark>>,
    settings: HarpoonSettings,
}

pub enum HarpoonEvent {
    MarksChanged,
}

impl EventEmitter<HarpoonEvent> for HarpoonStore {}

impl HarpoonStore {
    pub fn new(project: WeakEntity<Project>) -> Self {
        let settings = HarpoonSettings::default();
        let max_slots = settings.max_slots.unwrap_or(MAX_HARPOON_SLOTS);

        Self {
            project,
            marks: vec![None; max_slots],
            settings,
        }
    }

    pub fn mark_current_file(
        &mut self,
        project_path: ProjectPath,
        cx: &mut Context<Self>,
    ) -> Result<usize> {
                    // Check for duplicates
                    if let Some(existing_slot) = self.is_marked(&project_path) {
                        // Already marked, return the slot index
                            return Ok(existing_slot);
                        }
                        // Find the first empty slot
        // Find the first empty slot
        let slot = self
            .marks
            .iter()
            .position(|mark| mark.is_none())
            .unwrap_or(0); // If no empty slots, use slot 0

        let display_name = project_path
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Unknown")
            .to_string();

        let mark = HarpoonMark {
            project_path,
            display_name,
        };
        

        self.marks[slot] = Some(mark);
        cx.emit(HarpoonEvent::MarksChanged);
        cx.notify();

        Ok(slot)
    }

    pub fn get_mark(&self, slot: usize) -> Option<&HarpoonMark> {
        if slot < self.marks.len() {
            self.marks[slot].as_ref()
        } else {
            None
        }
    }

    pub fn remove_mark(&mut self, slot: usize, cx: &mut Context<Self>) -> bool {
        if slot < self.marks.len() && self.marks[slot].is_some() {
            self.marks[slot] = None;
            cx.emit(HarpoonEvent::MarksChanged);
            cx.notify();
            true
        } else {
            false
        }
    }

    pub fn clear_all(&mut self, cx: &mut Context<Self>) {
        self.marks.fill(None);
        cx.emit(HarpoonEvent::MarksChanged);
        cx.notify();
    }

    pub fn get_all_marks(&self) -> Vec<(usize, &HarpoonMark)> {
        self.marks
            .iter()
            .enumerate()
            .filter_map(|(i, mark)| mark.as_ref().map(|m| (i, m)))
            .collect()
    }

    pub fn is_marked(&self, project_path: &ProjectPath) -> Option<usize> {
        self.marks.iter().position(|mark| {
            mark.as_ref()
                .map(|m| m.project_path == *project_path)
                .unwrap_or(false)
        })
    }
}

// Global storage for harpoon marks per workspace
#[derive(Default)]
pub struct GlobalHarpoonStore {
    stores: HashMap<WeakEntity<Project>, Entity<HarpoonStore>>,
}

impl Global for GlobalHarpoonStore {}

pub fn init(cx: &mut App) {
    HarpoonSettings::register(cx);
    cx.set_global(GlobalHarpoonStore::default());
}

pub fn get_or_create_harpoon_store(
    project: &Entity<Project>,
    cx: &mut App,
) -> Entity<HarpoonStore> {
    let project_weak = project.downgrade();

    // First, check if we already have a store for this project
    {
        let global_store = cx.global_mut::<GlobalHarpoonStore>();

        // Clean up any dead project references
        global_store
            .stores
            .retain(|weak_project, _| weak_project.upgrade().is_some());

        if let Some(store) = global_store.stores.get(&project_weak) {
            return store.clone();
        }
    }

    // Create new store if we don't have one
    let store = cx.new(|_| HarpoonStore::new(project_weak.clone()));

    // Insert the new store
    let global_store = cx.global_mut::<GlobalHarpoonStore>();
    global_store.stores.insert(project_weak, store.clone());

    store
}

// Simple Harpoon Picker Implementation
pub struct HarpoonPicker {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    marks: Vec<(usize, HarpoonMark)>,
    selected_index: usize,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for HarpoonPicker {}
impl ModalView for HarpoonPicker {}

impl HarpoonPicker {
    pub fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let harpoon_store = get_or_create_harpoon_store(&project, cx);
        let marks = harpoon_store
            .read(cx)
            .get_all_marks()
            .into_iter()
            .map(|(slot, mark)| (slot, mark.clone()))
            .collect();

        Self {
            project,
            workspace,
            marks,
            selected_index: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn move_selection(&mut self, direction: i32, cx: &mut Context<Self>) {
        if self.marks.is_empty() {
            return;
        }

        let new_index = if direction > 0 {
            (self.selected_index + 1) % self.marks.len()
        } else {
            if self.selected_index == 0 {
                self.marks.len() - 1
            } else {
                self.selected_index - 1
            }
        };

        self.selected_index = new_index;
        cx.notify();
    }

    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, mark)) = self.marks.get(self.selected_index) {
            if let Some(workspace) = self.workspace.upgrade() {
                let project_path = mark.project_path.clone();
                let task = workspace.update(cx, |workspace, cx| {
                    workspace.open_path_preview(project_path, None, true, false, true, window, cx)
                });
                task.detach_and_log_err(cx);
            }
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for HarpoonPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HarpoonPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("HarpoonPicker")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                this.move_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.move_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| {
                this.cancel(cx);
            }))
            .w_96()
            .max_h_80()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .child(
                v_flex()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .items_center()
                            .justify_center()
                            .size_full()
                            .child(Label::new("Harpoon").color(Color::Accent)),
                    )
                    .when(self.marks.is_empty(), |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_4()
                                .child(Label::new("No files marked").color(Color::Muted)),
                        )
                    })
                    .when(!self.marks.is_empty(), |this| {
                        this.child(v_flex().children(self.marks.iter().enumerate().map(
                            |(ix, (slot, mark))| {
                                let selected = ix == self.selected_index;
                                div()
                                    .px_3()
                                    .py_1()
                                    .when(selected, |this| {
                                        this.bg(cx.theme().colors().element_selected)
                                    })
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(Label::new(format!("{}", slot + 1)).color(
                                                if selected {
                                                    Color::Selected
                                                } else {
                                                    Color::Muted
                                                },
                                            ))
                                            .child(
                                                Label::new(
                                                    mark.project_path
                                                        .path
                                                        .to_string_lossy()
                                                        .to_string(),
                                                )
                                                .color(if selected {
                                                    Color::Selected
                                                } else {
                                                    Color::Default
                                                }),
                                            ),
                                    )
                            },
                        )))
                    }),
            )
    }
}
