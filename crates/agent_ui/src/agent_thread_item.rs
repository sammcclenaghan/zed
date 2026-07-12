use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use project::{AgentId, Project};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{prelude::*, utils::WithRemSize};
use workspace::{
    ItemId, PathList, Workspace, WorkspaceId, delete_unloaded_items,
    item::{Item, ItemEvent, SerializableItem},
};

use crate::conversation_view::RootThreadUpdated;
use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore};
use crate::{Agent, AgentPanel, AgentThreadSource, ConversationView, StateChange};

/// A workspace tab hosting a single agent conversation. The wrapped
/// [`ConversationView`] is the same view type the agent panel displays, so a
/// thread can move between the panel and a tab without duplicating the
/// underlying thread.
pub struct AgentThreadItem {
    conversation_view: Entity<ConversationView>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

pub enum AgentThreadItemEvent {
    TitleUpdated,
}

impl EventEmitter<AgentThreadItemEvent> for AgentThreadItem {}

impl AgentThreadItem {
    pub fn new(
        conversation_view: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(
                &conversation_view,
                |_this, _view, _event: &StateChange, cx| {
                    cx.emit(AgentThreadItemEvent::TitleUpdated);
                    cx.notify();
                },
            ),
            cx.subscribe(
                &conversation_view,
                |_this, _view, _event: &RootThreadUpdated, cx| {
                    cx.emit(AgentThreadItemEvent::TitleUpdated);
                    cx.notify();
                },
            ),
        ];

        Self {
            conversation_view,
            workspace,
            _subscriptions: subscriptions,
        }
    }

    pub fn conversation_view(&self) -> &Entity<ConversationView> {
        &self.conversation_view
    }

    pub fn thread_id(&self, cx: &App) -> ThreadId {
        self.conversation_view.read(cx).thread_id
    }
}

impl Focusable for AgentThreadItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.read(cx).focus_handle(cx)
    }
}

impl Render for AgentThreadItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        WithRemSize::new(ThemeSettings::get_global(cx).agent_ui_font_size(cx))
            .size_full()
            .child(
                v_flex()
                    .size_full()
                    .bg(cx.theme().colors().panel_background)
                    .child(self.conversation_view.clone()),
            )
    }
}

impl Item for AgentThreadItem {
    type Event = AgentThreadItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).title(cx)
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let icon = self
            .conversation_view
            .read(cx)
            .agent_key()
            .icon()
            .unwrap_or(IconName::ZedAgent);
        Some(Icon::new(icon).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.conversation_view.read(cx).title(cx))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            AgentThreadItemEvent::TitleUpdated => f(ItemEvent::UpdateTab),
        }
    }

    fn include_in_nav_history() -> bool {
        true
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        // Closing the tab must not silently cancel an in-flight response:
        // hand the conversation view back to the agent panel, which keeps
        // running threads alive (and a bounded number of idle ones) in its
        // retained set. Dropping the view would close the agent session.
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        let conversation_view = self.conversation_view.clone();
        panel.update(cx, |panel, cx| {
            panel.retain_thread(conversation_view, cx);
        });
    }
}

impl SerializableItem for AgentThreadItem {
    fn serialized_item_kind() -> &'static str {
        "AgentThreadItem"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::AgentThreadItemDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "agent_thread_items", &db, cx)
    }

    fn deserialize(
        _project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::AgentThreadItemDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (thread_id, agent_id) = db
                .get_thread(item_id, workspace_id)?
                .context("no serialized agent thread item found")?;
            let agent = Agent::from(AgentId::new(agent_id));

            // The agent panel owns the per-workspace agent connections that
            // conversation views are built from, and panels are loaded
            // asynchronously alongside item deserialization, so wait for it
            // to appear before restoring the tab.
            const PANEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
            const PANEL_POLL_ATTEMPTS: usize = 100;
            let mut panel = None;
            for _ in 0..PANEL_POLL_ATTEMPTS {
                panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))?;
                if panel.is_some() {
                    break;
                }
                cx.background_executor().timer(PANEL_POLL_INTERVAL).await;
            }
            let panel = panel.context(
                "agent panel never became available; cannot restore agent thread tab",
            )?;

            let metadata = cx.update(|_window, cx| {
                ThreadMetadataStore::try_global(cx)
                    .and_then(|store| store.read(cx).entry(thread_id).cloned())
            })?;
            let metadata = metadata.with_context(|| {
                format!(
                    "agent thread {} is no longer in history; dropping its tab",
                    thread_id.to_key_string()
                )
            })?;

            panel.update_in(cx, |panel, window, cx| {
                let conversation_view = panel.thread_view_for_tab(
                    agent,
                    thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title(),
                    AgentThreadSource::Tab,
                    window,
                    cx,
                );
                cx.new(|cx| AgentThreadItem::new(conversation_view, workspace.clone(), cx))
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let conversation_view = self.conversation_view.read(cx);
        let thread_id = conversation_view.thread_id;
        let agent_id = conversation_view.agent_key().id().0.to_string();
        let db = persistence::AgentThreadItemDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_thread(item_id, workspace_id, thread_id, agent_id)
                .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        // The serialized state (thread id + agent) is fixed at creation;
        // the row is written once when the item is added to a pane.
        false
    }
}

/// Opens the given thread as a workspace tab in the active pane. If a tab for
/// this thread already exists in any pane, it is activated instead of opening
/// a duplicate.
pub fn open_agent_thread_in_workspace(
    workspace: &mut Workspace,
    agent: Agent,
    thread_id: ThreadId,
    work_dirs: Option<PathList>,
    title: Option<SharedString>,
    source: AgentThreadSource,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<AgentThreadItem>(cx)
        .find(|item| item.read(cx).thread_id(cx) == thread_id);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        log::error!("cannot open agent thread as a tab: agent panel is not available");
        return;
    };

    let conversation_view = panel.update(cx, |panel, cx| {
        panel.thread_view_for_tab(agent, thread_id, work_dirs, title, source, window, cx)
    });
    let item = cx.new(|cx| AgentThreadItem::new(conversation_view, workspace.weak_handle(), cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// Opens a brand-new thread for the panel's currently selected agent as a
/// workspace tab in the active pane.
pub fn open_new_agent_thread_in_workspace(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        log::error!("cannot open a new agent thread tab: agent panel is not available");
        return;
    };
    if !panel.read(cx).has_open_project(cx) {
        return;
    }
    panel.update(cx, |panel, cx| {
        panel.record_new_thread_tab_created(cx);
    });
    let agent = panel.read(cx).selected_agent(cx);
    open_agent_thread_in_workspace(
        workspace,
        agent,
        ThreadId::new(),
        None,
        None,
        AgentThreadSource::Tab,
        window,
        cx,
    );
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    use crate::thread_metadata_store::ThreadId;

    pub struct AgentThreadItemDb(ThreadSafeConnection);

    impl Domain for AgentThreadItemDb {
        const NAME: &str = stringify!(AgentThreadItemDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE agent_thread_items (
                workspace_id INTEGER,
                item_id INTEGER,
                thread_id BLOB NOT NULL,
                agent_id TEXT NOT NULL,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(AgentThreadItemDb, [WorkspaceDb]);

    impl AgentThreadItemDb {
        query! {
            pub async fn save_thread(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                thread_id: ThreadId,
                agent_id: String
            ) -> Result<()> {
                INSERT OR REPLACE INTO agent_thread_items(item_id, workspace_id, thread_id, agent_id)
                VALUES (?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_thread(
                item_id: ItemId,
                workspace_id: WorkspaceId
            ) -> Result<Option<(ThreadId, String)>> {
                SELECT thread_id, agent_id
                FROM agent_thread_items
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::init_test;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use workspace::MultiWorkspace;

    async fn setup_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<AgentPanel>,
        &mut gpui::VisualTestContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();

        (workspace, panel, cx)
    }

    #[gpui::test]
    async fn test_opening_thread_creates_tab_in_active_pane(cx: &mut TestAppContext) {
        let (workspace, _panel, cx) = setup_workspace_with_panel(cx).await;

        let thread_id = ThreadId::new();
        workspace.update_in(cx, |workspace, window, cx| {
            open_agent_thread_in_workspace(
                workspace,
                Agent::Stub,
                thread_id,
                None,
                None,
                AgentThreadSource::Tab,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let items = workspace
                .items_of_type::<AgentThreadItem>(cx)
                .collect::<Vec<_>>();
            assert_eq!(items.len(), 1, "expected exactly one agent thread tab");
            assert_eq!(items[0].read(cx).thread_id(cx), thread_id);

            let active = workspace
                .active_item_as::<AgentThreadItem>(cx)
                .expect("agent thread tab should be the active item");
            assert_eq!(active.entity_id(), items[0].entity_id());
        });
    }

    #[gpui::test]
    async fn test_opening_same_thread_twice_focuses_existing_tab(cx: &mut TestAppContext) {
        let (workspace, _panel, cx) = setup_workspace_with_panel(cx).await;

        let first_thread_id = ThreadId::new();
        workspace.update_in(cx, |workspace, window, cx| {
            open_agent_thread_in_workspace(
                workspace,
                Agent::Stub,
                first_thread_id,
                None,
                None,
                AgentThreadSource::Tab,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let first_item = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<AgentThreadItem>(cx)
                .expect("first thread tab should be active")
        });

        let second_thread_id = ThreadId::new();
        workspace.update_in(cx, |workspace, window, cx| {
            open_agent_thread_in_workspace(
                workspace,
                Agent::Stub,
                second_thread_id,
                None,
                None,
                AgentThreadSource::Tab,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let items = workspace
                .items_of_type::<AgentThreadItem>(cx)
                .collect::<Vec<_>>();
            assert_eq!(items.len(), 2, "expected a tab per thread");
            let active = workspace
                .active_item_as::<AgentThreadItem>(cx)
                .expect("second thread tab should be active");
            assert_eq!(active.read(cx).thread_id(cx), second_thread_id);
        });

        // Re-opening the first thread focuses its existing tab instead of
        // creating a duplicate.
        workspace.update_in(cx, |workspace, window, cx| {
            open_agent_thread_in_workspace(
                workspace,
                Agent::Stub,
                first_thread_id,
                None,
                None,
                AgentThreadSource::Tab,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let items = workspace
                .items_of_type::<AgentThreadItem>(cx)
                .collect::<Vec<_>>();
            assert_eq!(
                items.len(),
                2,
                "re-opening an open thread must not create a duplicate tab"
            );
            let active = workspace
                .active_item_as::<AgentThreadItem>(cx)
                .expect("first thread tab should be active again");
            assert_eq!(active.entity_id(), first_item.entity_id());
        });
    }
}
