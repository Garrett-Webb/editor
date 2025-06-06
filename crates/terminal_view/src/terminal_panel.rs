use std::{cmp, ops::ControlFlow, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    default_working_directory,
    persistence::{
        deserialize_terminal_panel, serialize_pane_group, SerializedItems, SerializedTerminalPanel,
    },
    TerminalView,
};
use breadcrumbs::Breadcrumbs;
use collections::HashMap;
use db::kvp::KEY_VALUE_STORE;
use futures::future::join_all;
use gpui::{
    actions, Action, AnchorCorner, AnyView, AppContext, AsyncWindowContext, Entity, EventEmitter,
    ExternalPaths, FocusHandle, FocusableView, IntoElement, Model, ParentElement, Pixels, Render,
    Styled, Task, View, ViewContext, VisualContext, WeakView, WindowContext,
};
use itertools::Itertools;
use project::{terminals::TerminalKind, Fs, Project, ProjectEntryId};
use search::{buffer_search::DivRegistrar, BufferSearchBar};
use settings::Settings;
use task::{RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal::{
    terminal_settings::{TerminalDockPosition, TerminalSettings},
    Terminal,
};
use ui::{
    prelude::*, ButtonCommon, Clickable, ContextMenu, FluentBuilder, PopoverMenu, Toggleable,
    Tooltip,
};
use util::{ResultExt, TryFutureExt};
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    item::SerializableItem,
    move_item, pane,
    ui::IconName,
    ActivateNextPane, ActivatePane, ActivatePaneInDirection, ActivatePreviousPane, DraggedTab,
    ItemId, NewTerminal, Pane, PaneGroup, SplitDirection, SplitDown, SplitLeft, SplitRight,
    SplitUp, SwapPaneInDirection, ToggleZoom, Workspace,
};

use anyhow::{anyhow, Context, Result};
use zed_actions::InlineAssist;

const TERMINAL_PANEL_KEY: &str = "TerminalPanel";

actions!(terminal_panel, [ToggleFocus]);

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(
        |workspace: &mut Workspace, _: &mut ViewContext<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
            workspace.register_action(TerminalPanel::open_terminal);
            workspace.register_action(|workspace, _: &ToggleFocus, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    workspace.toggle_panel_focus::<TerminalPanel>(cx);
                }
            });
        },
    )
    .detach();
}

pub struct TerminalPanel {
    pub(crate) active_pane: View<Pane>,
    pub(crate) center: PaneGroup,
    fs: Arc<dyn Fs>,
    workspace: WeakView<Workspace>,
    pub(crate) width: Option<Pixels>,
    pub(crate) height: Option<Pixels>,
    pending_serialization: Task<Option<()>>,
    pending_terminals_to_add: usize,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    assistant_tab_bar_button: Option<AnyView>,
}

impl TerminalPanel {
    pub fn new(workspace: &Workspace, cx: &mut ViewContext<Self>) -> Self {
        let project = workspace.project();
        let pane = new_terminal_pane(workspace.weak_handle(), project.clone(), false, cx);
        let center = PaneGroup::new(pane.clone());
        cx.focus_view(&pane);
        let terminal_panel = Self {
            center,
            active_pane: pane,
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            pending_serialization: Task::ready(None),
            width: None,
            height: None,
            pending_terminals_to_add: 0,
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            assistant_tab_bar_button: None,
        };
        terminal_panel.apply_tab_bar_buttons(&terminal_panel.active_pane, cx);
        terminal_panel
    }

    pub fn asssistant_enabled(&mut self, enabled: bool, cx: &mut ViewContext<Self>) {
        self.assistant_enabled = enabled;
        if enabled {
            let focus_handle = self
                .active_pane
                .read(cx)
                .active_item()
                .map(|item| item.focus_handle(cx))
                .unwrap_or(self.focus_handle(cx));
            self.assistant_tab_bar_button = Some(
                cx.new_view(move |_| InlineAssistTabBarButton { focus_handle })
                    .into(),
            );
        } else {
            self.assistant_tab_bar_button = None;
        }
        for pane in self.center.panes() {
            self.apply_tab_bar_buttons(pane, cx);
        }
    }

    fn apply_tab_bar_buttons(&self, terminal_pane: &View<Pane>, cx: &mut ViewContext<Self>) {
        terminal_pane.update(cx, |pane, cx| {
            pane.set_render_tab_bar_buttons(cx, move |pane, cx| {
                let split_context = pane
                    .active_item()
                    .and_then(|item| item.downcast::<TerminalView>())
                    .map(|terminal_view| terminal_view.read(cx).focus_handle.clone());
                if !pane.has_focus(cx) && !pane.context_menu_focused(cx) {
                    return (None, None);
                }
                let right_children = h_flex()
                    .gap(DynamicSpacing::Base02.rems(cx))
                    .child(
                        IconButton::new("New Terminal", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(|cx| Tooltip::for_action("New Terminal", &NewTerminal, cx))
                            .on_click(cx.listener(move |_, _, cx| {
                                cx.dispatch_action(workspace::NewTerminal.boxed_clone());
                            })),
                    )
                    .child(
                        PopoverMenu::new("terminal-pane-tab-bar-split")
                            .trigger(
                                IconButton::new("terminal-pane-split", IconName::Split)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|cx| Tooltip::text("Split Pane", cx)),
                            )
                            .anchor(AnchorCorner::TopRight)
                            .with_handle(pane.split_item_context_menu_handle.clone())
                            .menu({
                                let split_context = split_context.clone();
                                move |cx| {
                                    ContextMenu::build(cx, |menu, _| {
                                        menu.when_some(
                                            split_context.clone(),
                                            |menu, split_context| menu.context(split_context),
                                        )
                                        .action("Split Right", SplitRight.boxed_clone())
                                        .action("Split Left", SplitLeft.boxed_clone())
                                        .action("Split Up", SplitUp.boxed_clone())
                                        .action("Split Down", SplitDown.boxed_clone())
                                    })
                                    .into()
                                }
                            }),
                    )
                    .child({
                        let zoomed = pane.is_zoomed();
                        IconButton::new("toggle_zoom", IconName::Maximize)
                            .icon_size(IconSize::Small)
                            .toggle_state(zoomed)
                            .selected_icon(IconName::Minimize)
                            .on_click(cx.listener(|pane, _, cx| {
                                pane.toggle_zoom(&workspace::ToggleZoom, cx);
                            }))
                            .tooltip(move |cx| {
                                Tooltip::for_action(
                                    if zoomed { "Zoom Out" } else { "Zoom In" },
                                    &ToggleZoom,
                                    cx,
                                )
                            })
                    })
                    .into_any_element()
                    .into();
                (None, right_children)
            });
        });
    }

    pub async fn load(
        workspace: WeakView<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<View<Self>> {
        let serialized_panel = cx
            .background_executor()
            .spawn(async move { KEY_VALUE_STORE.read_kvp(TERMINAL_PANEL_KEY) })
            .await
            .log_err()
            .flatten()
            .map(|panel| serde_json::from_str::<SerializedTerminalPanel>(&panel))
            .transpose()
            .log_err()
            .flatten();

        let terminal_panel = workspace
            .update(&mut cx, |workspace, cx| {
                match serialized_panel.zip(workspace.database_id()) {
                    Some((serialized_panel, database_id)) => deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        database_id,
                        serialized_panel,
                        cx,
                    ),
                    None => Task::ready(Ok(cx.new_view(|cx| TerminalPanel::new(workspace, cx)))),
                }
            })?
            .await?;

        if let Some(workspace) = workspace.upgrade() {
            terminal_panel
                .update(&mut cx, |_, cx| {
                    cx.subscribe(&workspace, |terminal_panel, _, e, cx| {
                        if let workspace::Event::SpawnTask {
                            action: spawn_in_terminal,
                        } = e
                        {
                            terminal_panel.spawn_task(spawn_in_terminal, cx);
                        };
                    })
                    .detach();
                })
                .ok();
        }

        // Since panels/docks are loaded outside from the workspace, we cleanup here, instead of through the workspace.
        if let Some(workspace) = workspace.upgrade() {
            let cleanup_task = workspace.update(&mut cx, |workspace, cx| {
                let alive_item_ids = terminal_panel
                    .read(cx)
                    .center
                    .panes()
                    .into_iter()
                    .flat_map(|pane| pane.read(cx).items())
                    .map(|item| item.item_id().as_u64() as ItemId)
                    .collect();
                workspace
                    .database_id()
                    .map(|workspace_id| TerminalView::cleanup(workspace_id, alive_item_ids, cx))
            })?;
            if let Some(task) = cleanup_task {
                task.await.log_err();
            }
        }

        Ok(terminal_panel)
    }

    fn handle_pane_event(
        &mut self,
        pane: View<Pane>,
        event: &pane::Event,
        cx: &mut ViewContext<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { .. } => self.serialize(cx),
            pane::Event::RemovedItem { .. } => self.serialize(cx),
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                let _removal_result = self.center.remove(&pane);
                if pane_count_before_removal == 1 {
                    self.center.first_pane().update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    });
                    cx.emit(PanelEvent::Close);
                } else {
                    if let Some(focus_on_pane) =
                        focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                    {
                        focus_on_pane.focus_handle(cx).focus(cx);
                    }
                }
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(true, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), cx)
                    })
                }
                self.serialize(cx);
            }
            pane::Event::Split(direction) => {
                let new_pane = self.new_pane_with_cloned_active_terminal(cx);
                let pane = pane.clone();
                let direction = *direction;
                cx.spawn(move |terminal_panel, mut cx| async move {
                    let Some(new_pane) = new_pane.await else {
                        return;
                    };
                    terminal_panel
                        .update(&mut cx, |terminal_panel, cx| {
                            terminal_panel
                                .center
                                .split(&pane, &new_pane, direction)
                                .log_err();
                            cx.focus_view(&new_pane);
                        })
                        .ok();
                })
                .detach();
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
            }

            _ => {}
        }
    }

    fn new_pane_with_cloned_active_terminal(
        &mut self,
        cx: &mut ViewContext<Self>,
    ) -> Task<Option<View<Pane>>> {
        let Some(workspace) = self.workspace.clone().upgrade() else {
            return Task::ready(None);
        };
        let database_id = workspace.read(cx).database_id();
        let weak_workspace = self.workspace.clone();
        let project = workspace.read(cx).project().clone();
        let working_directory = self
            .active_pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<TerminalView>())
            .and_then(|terminal_view| {
                terminal_view
                    .read(cx)
                    .terminal()
                    .read(cx)
                    .working_directory()
            })
            .or_else(|| default_working_directory(workspace.read(cx), cx));
        let kind = TerminalKind::Shell(working_directory);
        let window = cx.window_handle();
        cx.spawn(move |terminal_panel, mut cx| async move {
            let terminal = project
                .update(&mut cx, |project, cx| {
                    project.create_terminal(kind, window, cx)
                })
                .log_err()?
                .await
                .log_err()?;

            let terminal_view = Box::new(
                cx.new_view(|cx| {
                    TerminalView::new(terminal.clone(), weak_workspace.clone(), database_id, cx)
                })
                .ok()?,
            );
            let pane = terminal_panel
                .update(&mut cx, |terminal_panel, cx| {
                    let pane = new_terminal_pane(
                        weak_workspace,
                        project,
                        terminal_panel.active_pane.read(cx).is_zoomed(),
                        cx,
                    );
                    terminal_panel.apply_tab_bar_buttons(&pane, cx);
                    pane
                })
                .ok()?;

            pane.update(&mut cx, |pane, cx| {
                pane.add_item(terminal_view, true, true, None, cx);
            })
            .ok()?;

            Some(pane)
        })
    }

    pub fn open_terminal(
        workspace: &mut Workspace,
        action: &workspace::OpenTerminal,
        cx: &mut ViewContext<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| {
                panel.add_terminal(
                    TerminalKind::Shell(Some(action.working_directory.clone())),
                    RevealStrategy::Always,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    fn spawn_task(&mut self, spawn_in_terminal: &SpawnInTerminal, cx: &mut ViewContext<Self>) {
        let mut spawn_task = spawn_in_terminal.clone();
        let Ok(is_local) = self
            .workspace
            .update(cx, |workspace, cx| workspace.project().read(cx).is_local())
        else {
            return;
        };
        if let ControlFlow::Break(_) =
            Self::fill_command(is_local, spawn_in_terminal, &mut spawn_task)
        {
            return;
        }
        let spawn_task = spawn_task;

        let allow_concurrent_runs = spawn_in_terminal.allow_concurrent_runs;
        let use_new_terminal = spawn_in_terminal.use_new_terminal;

        if allow_concurrent_runs && use_new_terminal {
            self.spawn_in_new_terminal(spawn_task, cx)
                .detach_and_log_err(cx);
            return;
        }

        let terminals_for_task = self.terminals_for_task(&spawn_in_terminal.full_label, cx);
        if terminals_for_task.is_empty() {
            self.spawn_in_new_terminal(spawn_task, cx)
                .detach_and_log_err(cx);
            return;
        }
        let (existing_item_index, task_pane, existing_terminal) = terminals_for_task
            .last()
            .expect("covered no terminals case above")
            .clone();
        let id = spawn_in_terminal.id.clone();
        cx.spawn(move |this, mut cx| async move {
            if allow_concurrent_runs {
                debug_assert!(
                    !use_new_terminal,
                    "Should have handled 'allow_concurrent_runs && use_new_terminal' case above"
                );
                this.update(&mut cx, |terminal_panel, cx| {
                    terminal_panel.replace_terminal(
                        spawn_task,
                        task_pane,
                        existing_item_index,
                        existing_terminal,
                        cx,
                    )
                })?
                .await;
            } else {
                this.update(&mut cx, |this, cx| {
                    this.deferred_tasks.insert(
                        id,
                        cx.spawn(|terminal_panel, mut cx| async move {
                            wait_for_terminals_tasks(terminals_for_task, &mut cx).await;
                            let Ok(Some(new_terminal_task)) =
                                terminal_panel.update(&mut cx, |terminal_panel, cx| {
                                    if use_new_terminal {
                                        terminal_panel
                                            .spawn_in_new_terminal(spawn_task, cx)
                                            .detach_and_log_err(cx);
                                        None
                                    } else {
                                        Some(terminal_panel.replace_terminal(
                                            spawn_task,
                                            task_pane,
                                            existing_item_index,
                                            existing_terminal,
                                            cx,
                                        ))
                                    }
                                })
                            else {
                                return;
                            };
                            new_terminal_task.await;
                        }),
                    );
                })
                .ok();
            }
            anyhow::Result::<_, anyhow::Error>::Ok(())
        })
        .detach()
    }

    pub fn fill_command(
        is_local: bool,
        spawn_in_terminal: &SpawnInTerminal,
        spawn_task: &mut SpawnInTerminal,
    ) -> ControlFlow<()> {
        let Some((shell, mut user_args)) = (match spawn_in_terminal.shell.clone() {
            Shell::System => {
                if is_local {
                    retrieve_system_shell().map(|shell| (shell, Vec::new()))
                } else {
                    Some(("\"${SHELL:-sh}\"".to_string(), Vec::new()))
                }
            }
            Shell::Program(shell) => Some((shell, Vec::new())),
            Shell::WithArguments { program, args, .. } => Some((program, args)),
        }) else {
            return ControlFlow::Break(());
        };
        #[cfg(target_os = "windows")]
        let windows_shell_type = to_windows_shell_type(&shell);
        #[cfg(not(target_os = "windows"))]
        {
            spawn_task.command_label = format!("{shell} -i -c '{}'", spawn_task.command_label);
        }
        #[cfg(target_os = "windows")]
        {
            use crate::terminal_panel::WindowsShellType;

            match windows_shell_type {
                WindowsShellType::Powershell => {
                    spawn_task.command_label = format!("{shell} -C '{}'", spawn_task.command_label)
                }
                WindowsShellType::Cmd => {
                    spawn_task.command_label = format!("{shell} /C '{}'", spawn_task.command_label)
                }
                WindowsShellType::Other => {
                    spawn_task.command_label =
                        format!("{shell} -i -c '{}'", spawn_task.command_label)
                }
            }
        }
        let task_command = std::mem::replace(&mut spawn_task.command, shell);
        let task_args = std::mem::take(&mut spawn_task.args);
        let combined_command = task_args
            .into_iter()
            .fold(task_command, |mut command, arg| {
                command.push(' ');
                #[cfg(not(target_os = "windows"))]
                command.push_str(&arg);
                #[cfg(target_os = "windows")]
                command.push_str(&to_windows_shell_variable(windows_shell_type, arg));
                command
            });
        #[cfg(not(target_os = "windows"))]
        user_args.extend(["-i".to_owned(), "-c".to_owned(), combined_command]);
        #[cfg(target_os = "windows")]
        {
            use crate::terminal_panel::WindowsShellType;

            match windows_shell_type {
                WindowsShellType::Powershell => {
                    user_args.extend(["-C".to_owned(), combined_command])
                }
                WindowsShellType::Cmd => user_args.extend(["/C".to_owned(), combined_command]),
                WindowsShellType::Other => {
                    user_args.extend(["-i".to_owned(), "-c".to_owned(), combined_command])
                }
            }
        }
        spawn_task.args = user_args;
        // Set up shell args unconditionally, as tasks are always spawned inside of a shell.

        ControlFlow::Continue(())
    }

    pub fn spawn_in_new_terminal(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<Model<Terminal>>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        let kind = TerminalKind::Task(spawn_task);
        match reveal_target {
            RevealTarget::Center => self
                .workspace
                .update(cx, |workspace, cx| {
                    Self::add_center_terminal(workspace, kind, cx)
                })
                .unwrap_or_else(|e| Task::ready(Err(e))),
            RevealTarget::Dock => self.add_terminal(kind, reveal, cx),
        }
    }

    /// Create a new Terminal in the current working directory or the user's home directory
    fn new_terminal(
        workspace: &mut Workspace,
        _: &workspace::NewTerminal,
        cx: &mut ViewContext<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        let kind = TerminalKind::Shell(default_working_directory(workspace, cx));

        terminal_panel
            .update(cx, |this, cx| {
                this.add_terminal(kind, RevealStrategy::Always, cx)
            })
            .detach_and_log_err(cx);
    }

    fn terminals_for_task(
        &self,
        label: &str,
        cx: &mut AppContext,
    ) -> Vec<(usize, View<Pane>, View<TerminalView>)> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };

        let pane_terminal_views = |pane: View<Pane>| {
            pane.read(cx)
                .items()
                .enumerate()
                .filter_map(|(index, item)| Some((index, item.act_as::<TerminalView>(cx)?)))
                .filter_map(|(index, terminal_view)| {
                    let task_state = terminal_view.read(cx).terminal().read(cx).task()?;
                    if &task_state.full_label == label {
                        Some((index, terminal_view))
                    } else {
                        None
                    }
                })
                .map(move |(index, terminal_view)| (index, pane.clone(), terminal_view))
        };

        self.center
            .panes()
            .into_iter()
            .cloned()
            .flat_map(pane_terminal_views)
            .chain(
                workspace
                    .read(cx)
                    .panes()
                    .into_iter()
                    .cloned()
                    .flat_map(pane_terminal_views),
            )
            .sorted_by_key(|(_, _, terminal_view)| terminal_view.entity_id())
            .collect()
    }

    fn activate_terminal_view(
        &self,
        pane: &View<Pane>,
        item_index: usize,
        focus: bool,
        cx: &mut WindowContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.activate_item(item_index, true, focus, cx)
        })
    }

    pub fn add_center_terminal(
        workspace: &mut Workspace,
        kind: TerminalKind,
        cx: &mut ViewContext<Workspace>,
    ) -> Task<Result<Model<Terminal>>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let window = cx.window_handle();
        let project = workspace.project().downgrade();
        cx.spawn(move |workspace, mut cx| async move {
            let terminal = project
                .update(&mut cx, |project, cx| {
                    project.create_terminal(kind, window, cx)
                })?
                .await?;

            workspace.update(&mut cx, |workspace, cx| {
                let view = cx.new_view(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, cx);
            })?;
            Ok(terminal)
        })
    }

    fn add_terminal(
        &mut self,
        kind: TerminalKind,
        reveal_strategy: RevealStrategy,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<Model<Terminal>>> {
        if !self.is_enabled(cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }

        let workspace = self.workspace.clone();
        self.pending_terminals_to_add += 1;

        cx.spawn(|terminal_panel, mut cx| async move {
            let pane = terminal_panel.update(&mut cx, |this, _| this.active_pane.clone())?;
            let project = workspace.update(&mut cx, |workspace, _| workspace.project().clone())?;
            let window = cx.window_handle();
            let terminal = project
                .update(&mut cx, |project, cx| {
                    project.create_terminal(kind, window, cx)
                })?
                .await?;
            let result = workspace.update(&mut cx, |workspace, cx| {
                let terminal_view = Box::new(cx.new_view(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        cx,
                    )
                }));
                pane.update(cx, |pane, cx| {
                    let focus = pane.has_focus(cx);
                    pane.add_item(terminal_view, true, focus, None, cx);
                });

                match reveal_strategy {
                    RevealStrategy::Always => {
                        workspace.focus_panel::<Self>(cx);
                    }
                    RevealStrategy::NoFocus => {
                        workspace.open_panel::<Self>(cx);
                    }
                    RevealStrategy::Never => {}
                }
                Ok(terminal)
            })?;
            terminal_panel.update(&mut cx, |this, cx| {
                this.pending_terminals_to_add = this.pending_terminals_to_add.saturating_sub(1);
                this.serialize(cx)
            })?;
            result
        })
    }

    fn serialize(&mut self, cx: &mut ViewContext<Self>) {
        let height = self.height;
        let width = self.width;
        self.pending_serialization = cx.spawn(|terminal_panel, mut cx| async move {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let terminal_panel = terminal_panel.upgrade()?;
            let items = terminal_panel
                .update(&mut cx, |terminal_panel, cx| {
                    SerializedItems::WithSplits(serialize_pane_group(
                        &terminal_panel.center,
                        &terminal_panel.active_pane,
                        cx,
                    ))
                })
                .ok()?;
            cx.background_executor()
                .spawn(
                    async move {
                        KEY_VALUE_STORE
                            .write_kvp(
                                TERMINAL_PANEL_KEY.into(),
                                serde_json::to_string(&SerializedTerminalPanel {
                                    items,
                                    active_item_id: None,
                                    height,
                                    width,
                                })?,
                            )
                            .await?;
                        anyhow::Ok(())
                    }
                    .log_err(),
                )
                .await;
            Some(())
        });
    }

    fn replace_terminal(
        &self,
        spawn_task: SpawnInTerminal,
        task_pane: View<Pane>,
        terminal_item_index: usize,
        terminal_to_replace: View<TerminalView>,
        cx: &mut ViewContext<'_, Self>,
    ) -> Task<Option<()>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        let window = cx.window_handle();
        let task_workspace = self.workspace.clone();
        cx.spawn(move |terminal_panel, mut cx| async move {
            let project = terminal_panel
                .update(&mut cx, |this, cx| {
                    this.workspace
                        .update(cx, |workspace, _| workspace.project().clone())
                        .ok()
                })
                .ok()
                .flatten()?;
            let new_terminal = project
                .update(&mut cx, |project, cx| {
                    project.create_terminal(TerminalKind::Task(spawn_task), window, cx)
                })
                .ok()?
                .await
                .log_err()?;
            terminal_to_replace
                .update(&mut cx, |terminal_to_replace, cx| {
                    terminal_to_replace.set_terminal(new_terminal, cx);
                })
                .ok()?;

            match reveal {
                RevealStrategy::Always => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace
                            .update(&mut cx, |workspace, cx| {
                                workspace
                                    .active_item(cx)
                                    .context("retrieving active terminal item in the workspace")
                                    .log_err()?
                                    .focus_handle(cx)
                                    .focus(cx);
                                Some(())
                            })
                            .ok()??;
                    }
                    RevealTarget::Dock => {
                        terminal_panel
                            .update(&mut cx, |terminal_panel, cx| {
                                terminal_panel.activate_terminal_view(
                                    &task_pane,
                                    terminal_item_index,
                                    true,
                                    cx,
                                )
                            })
                            .ok()?;

                        cx.spawn(|mut cx| async move {
                            task_workspace
                                .update(&mut cx, |workspace, cx| workspace.focus_panel::<Self>(cx))
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::NoFocus => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace
                            .update(&mut cx, |workspace, cx| {
                                workspace.active_pane().focus_handle(cx).focus(cx);
                            })
                            .ok()?;
                    }
                    RevealTarget::Dock => {
                        terminal_panel
                            .update(&mut cx, |terminal_panel, cx| {
                                terminal_panel.activate_terminal_view(
                                    &task_pane,
                                    terminal_item_index,
                                    false,
                                    cx,
                                )
                            })
                            .ok()?;

                        cx.spawn(|mut cx| async move {
                            task_workspace
                                .update(&mut cx, |workspace, cx| workspace.open_panel::<Self>(cx))
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::Never => {}
            }

            Some(())
        })
    }

    fn has_no_terminals(&self, cx: &WindowContext) -> bool {
        self.active_pane.read(cx).items_len() == 0 && self.pending_terminals_to_add == 0
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    fn is_enabled(&self, cx: &WindowContext) -> bool {
        self.workspace.upgrade().map_or(false, |workspace| {
            is_enabled_in_workspace(workspace.read(cx), cx)
        })
    }
}

fn is_enabled_in_workspace(workspace: &Workspace, cx: &WindowContext) -> bool {
    workspace.project().read(cx).supports_terminal(cx)
}

pub fn new_terminal_pane(
    workspace: WeakView<Workspace>,
    project: Model<Project>,
    zoomed: bool,
    cx: &mut ViewContext<TerminalPanel>,
) -> View<Pane> {
    let is_local = project.read(cx).is_local();
    let terminal_panel = cx.view().clone();
    let pane = cx.new_view(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            NewTerminal.boxed_clone(),
            cx,
        );
        pane.set_zoomed(zoomed, cx);
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_| true);
        pane.set_zoom_out_on_close(false);

        let terminal_panel_for_split_check = terminal_panel.clone();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, cx| {
            if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                let current_pane = cx.view().clone();
                let can_drag_away =
                    terminal_panel_for_split_check.update(cx, |terminal_panel, _| {
                        let current_panes = terminal_panel.center.panes();
                        !current_panes.contains(&&tab.pane)
                            || current_panes.len() > 1
                            || (tab.pane != current_pane || pane.items_len() > 1)
                    });
                if can_drag_away {
                    let item = if tab.pane == current_pane {
                        pane.item_for_index(tab.ix)
                    } else {
                        tab.pane.read(cx).item_for_index(tab.ix)
                    };
                    if let Some(item) = item {
                        return item.downcast::<TerminalView>().is_some();
                    }
                }
            }
            false
        })));

        let buffer_search_bar = cx.new_view(search::BufferSearchBar::new);
        let breadcrumbs = cx.new_view(|_| Breadcrumbs::new());
        pane.toolbar().update(cx, |toolbar, cx| {
            toolbar.add_item(buffer_search_bar, cx);
            toolbar.add_item(breadcrumbs, cx);
        });

        pane.set_custom_drop_handle(cx, move |pane, dropped_item, cx| {
            if let Some(tab) = dropped_item.downcast_ref::<DraggedTab>() {
                let this_pane = cx.view().clone();
                let item = if tab.pane == this_pane {
                    pane.item_for_index(tab.ix)
                } else {
                    tab.pane.read(cx).item_for_index(tab.ix)
                };
                if let Some(item) = item {
                    if item.downcast::<TerminalView>().is_some() {
                        let source = tab.pane.clone();
                        let item_id_to_move = item.item_id();

                        let new_split_pane = pane
                            .drag_split_direction()
                            .map(|split_direction| {
                                terminal_panel.update(cx, |terminal_panel, cx| {
                                    let is_zoomed = if terminal_panel.active_pane == this_pane {
                                        pane.is_zoomed()
                                    } else {
                                        terminal_panel.active_pane.read(cx).is_zoomed()
                                    };
                                    let new_pane = new_terminal_pane(
                                        workspace.clone(),
                                        project.clone(),
                                        is_zoomed,
                                        cx,
                                    );
                                    terminal_panel.apply_tab_bar_buttons(&new_pane, cx);
                                    terminal_panel.center.split(
                                        &this_pane,
                                        &new_pane,
                                        split_direction,
                                    )?;
                                    anyhow::Ok(new_pane)
                                })
                            })
                            .transpose();

                        match new_split_pane {
                            // Source pane may be the one currently updated, so defer the move.
                            Ok(Some(new_pane)) => cx
                                .spawn(|_, mut cx| async move {
                                    cx.update(|cx| {
                                        move_item(
                                            &source,
                                            &new_pane,
                                            item_id_to_move,
                                            new_pane.read(cx).active_item_index(),
                                            cx,
                                        );
                                    })
                                    .ok();
                                })
                                .detach(),
                            // If we drop into existing pane or current pane,
                            // regular pane drop handler will take care of it,
                            // using the right tab index for the operation.
                            Ok(None) => return ControlFlow::Continue(()),
                            err @ Err(_) => {
                                err.log_err();
                                return ControlFlow::Break(());
                            }
                        };
                    } else if let Some(project_path) = item.project_path(cx) {
                        if let Some(entry_path) = project.read(cx).absolute_path(&project_path, cx)
                        {
                            add_paths_to_terminal(pane, &[entry_path], cx);
                        }
                    }
                }
            } else if let Some(&entry_id) = dropped_item.downcast_ref::<ProjectEntryId>() {
                if let Some(entry_path) = project
                    .read(cx)
                    .path_for_entry(entry_id, cx)
                    .and_then(|project_path| project.read(cx).absolute_path(&project_path, cx))
                {
                    add_paths_to_terminal(pane, &[entry_path], cx);
                }
            } else if is_local {
                if let Some(paths) = dropped_item.downcast_ref::<ExternalPaths>() {
                    add_paths_to_terminal(pane, paths.paths(), cx);
                }
            }

            ControlFlow::Break(())
        });

        pane
    });

    cx.subscribe(&pane, TerminalPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

async fn wait_for_terminals_tasks(
    terminals_for_task: Vec<(usize, View<Pane>, View<TerminalView>)>,
    cx: &mut AsyncWindowContext,
) {
    let pending_tasks = terminals_for_task.iter().filter_map(|(_, _, terminal)| {
        terminal
            .update(cx, |terminal_view, cx| {
                terminal_view
                    .terminal()
                    .update(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
            })
            .ok()
    });
    let _: Vec<()> = join_all(pending_tasks).await;
}

fn add_paths_to_terminal(pane: &mut Pane, paths: &[PathBuf], cx: &mut ViewContext<'_, Pane>) {
    if let Some(terminal_view) = pane
        .active_item()
        .and_then(|item| item.downcast::<TerminalView>())
    {
        cx.focus_view(&terminal_view);
        let mut new_text = paths.iter().map(|path| format!(" {path:?}")).join("");
        new_text.push(' ');
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.terminal().update(cx, |terminal, _| {
                terminal.paste(&new_text);
            });
        });
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Render for TerminalPanel {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let mut registrar = DivRegistrar::new(
            |panel, cx| {
                panel
                    .active_pane
                    .read(cx)
                    .toolbar()
                    .read(cx)
                    .item_of_type::<BufferSearchBar>()
            },
            cx,
        );
        BufferSearchBar::register(&mut registrar);
        let registrar = registrar.into_div();
        self.workspace
            .update(cx, |workspace, cx| {
                registrar.size_full().child(self.center.render(
                    workspace.project(),
                    &HashMap::default(),
                    None,
                    &self.active_pane,
                    workspace.zoomed_item(),
                    workspace.app_state(),
                    cx,
                ))
            })
            .ok()
            .map(|div| {
                div.on_action({
                    cx.listener(|terminal_panel, action: &ActivatePaneInDirection, cx| {
                        if let Some(pane) = terminal_panel.center.find_pane_in_direction(
                            &terminal_panel.active_pane,
                            action.0,
                            cx,
                        ) {
                            cx.focus_view(&pane);
                        } else {
                            terminal_panel
                                .workspace
                                .update(cx, |workspace, cx| {
                                    workspace.activate_pane_in_direction(action.0, cx)
                                })
                                .ok();
                        }
                    })
                })
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivateNextPane, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let next_ix = (ix + 1) % panes.len();
                            let next_pane = panes[next_ix].clone();
                            cx.focus_view(&next_pane);
                        }
                    }),
                )
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivatePreviousPane, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                            let prev_pane = panes[prev_ix].clone();
                            cx.focus_view(&prev_pane);
                        }
                    }),
                )
                .on_action(cx.listener(|terminal_panel, action: &ActivatePane, cx| {
                    let panes = terminal_panel.center.panes();
                    if let Some(pane) = panes.get(action.0).map(|p| (*p).clone()) {
                        cx.focus_view(&pane);
                    } else {
                        let new_pane = terminal_panel.new_pane_with_cloned_active_terminal(cx);
                        cx.spawn(|terminal_panel, mut cx| async move {
                            if let Some(new_pane) = new_pane.await {
                                terminal_panel
                                    .update(&mut cx, |terminal_panel, cx| {
                                        terminal_panel
                                            .center
                                            .split(
                                                &terminal_panel.active_pane,
                                                &new_pane,
                                                SplitDirection::Right,
                                            )
                                            .log_err();
                                        cx.focus_view(&new_pane);
                                    })
                                    .ok();
                            }
                        })
                        .detach();
                    }
                }))
                .on_action(cx.listener(
                    |terminal_panel, action: &SwapPaneInDirection, cx| {
                        if let Some(to) = terminal_panel
                            .center
                            .find_pane_in_direction(&terminal_panel.active_pane, action.0, cx)
                            .cloned()
                        {
                            terminal_panel
                                .center
                                .swap(&terminal_panel.active_pane.clone(), &to);
                            cx.notify();
                        }
                    },
                ))
            })
            .unwrap_or_else(|| div())
    }
}

impl FocusableView for TerminalPanel {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }
}

impl Panel for TerminalPanel {
    fn position(&self, cx: &WindowContext) -> DockPosition {
        match TerminalSettings::get_global(cx).dock {
            TerminalDockPosition::Left => DockPosition::Left,
            TerminalDockPosition::Bottom => DockPosition::Bottom,
            TerminalDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, cx: &mut ViewContext<Self>) {
        settings::update_settings_file::<TerminalSettings>(
            self.fs.clone(),
            cx,
            move |settings, _| {
                let dock = match position {
                    DockPosition::Left => TerminalDockPosition::Left,
                    DockPosition::Bottom => TerminalDockPosition::Bottom,
                    DockPosition::Right => TerminalDockPosition::Right,
                };
                settings.dock = Some(dock);
            },
        );
    }

    fn size(&self, cx: &WindowContext) -> Pixels {
        let settings = TerminalSettings::get_global(cx);
        match self.position(cx) {
            DockPosition::Left | DockPosition::Right => {
                self.width.unwrap_or(settings.default_width)
            }
            DockPosition::Bottom => self.height.unwrap_or(settings.default_height),
        }
    }

    fn set_size(&mut self, size: Option<Pixels>, cx: &mut ViewContext<Self>) {
        match self.position(cx) {
            DockPosition::Left | DockPosition::Right => self.width = size,
            DockPosition::Bottom => self.height = size,
        }
        self.serialize(cx);
        cx.notify();
    }

    fn is_zoomed(&self, cx: &WindowContext) -> bool {
        self.active_pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, cx: &mut ViewContext<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| {
                pane.set_zoomed(zoomed, cx);
            })
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, cx: &mut ViewContext<Self>) {
        if !active || !self.has_no_terminals(cx) {
            return;
        }
        cx.defer(|this, cx| {
            let Ok(kind) = this.workspace.update(cx, |workspace, cx| {
                TerminalKind::Shell(default_working_directory(workspace, cx))
            }) else {
                return;
            };

            this.add_terminal(kind, RevealStrategy::Always, cx)
                .detach_and_log_err(cx)
        })
    }

    fn icon_label(&self, cx: &WindowContext) -> Option<String> {
        let count = self
            .center
            .panes()
            .into_iter()
            .map(|pane| pane.read(cx).items_len())
            .sum::<usize>();
        if count == 0 {
            None
        } else {
            Some(count.to_string())
        }
    }

    fn persistent_name() -> &'static str {
        "TerminalPanel"
    }

    fn icon(&self, cx: &WindowContext) -> Option<IconName> {
        if (self.is_enabled(cx) || !self.has_no_terminals(cx))
            && TerminalSettings::get_global(cx).button
        {
            Some(IconName::Terminal)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _cx: &WindowContext) -> Option<&'static str> {
        Some("Terminal Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn pane(&self) -> Option<View<Pane>> {
        Some(self.active_pane.clone())
    }
}

struct InlineAssistTabBarButton {
    focus_handle: FocusHandle,
}

impl Render for InlineAssistTabBarButton {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle.clone();
        IconButton::new("terminal_inline_assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .on_click(cx.listener(|_, _, cx| {
                cx.dispatch_action(InlineAssist::default().boxed_clone());
            }))
            .tooltip(move |cx| {
                Tooltip::for_action_in("Inline Assist", &InlineAssist::default(), &focus_handle, cx)
            })
    }
}

fn retrieve_system_shell() -> Option<String> {
    #[cfg(not(target_os = "windows"))]
    {
        use anyhow::Context;
        use util::ResultExt;

        std::env::var("SHELL")
            .context("Error finding SHELL in env.")
            .log_err()
    }
    // `alacritty_terminal` uses this as default on Windows. See:
    // https://github.com/alacritty/alacritty/blob/0d4ab7bca43213d96ddfe40048fc0f922543c6f8/alacritty_terminal/src/tty/windows/mod.rs#L130
    #[cfg(target_os = "windows")]
    return Some("powershell".to_owned());
}

#[cfg(target_os = "windows")]
fn to_windows_shell_variable(shell_type: WindowsShellType, input: String) -> String {
    match shell_type {
        WindowsShellType::Powershell => to_powershell_variable(input),
        WindowsShellType::Cmd => to_cmd_variable(input),
        WindowsShellType::Other => input,
    }
}

#[cfg(target_os = "windows")]
fn to_windows_shell_type(shell: &str) -> WindowsShellType {
    if shell == "powershell"
        || shell.ends_with("powershell.exe")
        || shell == "pwsh"
        || shell.ends_with("pwsh.exe")
    {
        WindowsShellType::Powershell
    } else if shell == "cmd" || shell.ends_with("cmd.exe") {
        WindowsShellType::Cmd
    } else {
        // Someother shell detected, the user might install and use a
        // unix-like shell.
        WindowsShellType::Other
    }
}

/// Convert `${SOME_VAR}`, `$SOME_VAR` to `%SOME_VAR%`.
#[inline]
#[cfg(target_os = "windows")]
fn to_cmd_variable(input: String) -> String {
    if let Some(var_str) = input.strip_prefix("${") {
        if var_str.find(':').is_none() {
            // If the input starts with "${", remove the trailing "}"
            format!("%{}%", &var_str[..var_str.len() - 1])
        } else {
            // `${SOME_VAR:-SOME_DEFAULT}`, we currently do not handle this situation,
            // which will result in the task failing to run in such cases.
            input
        }
    } else if let Some(var_str) = input.strip_prefix('$') {
        // If the input starts with "$", directly append to "$env:"
        format!("%{}%", var_str)
    } else {
        // If no prefix is found, return the input as is
        input
    }
}

/// Convert `${SOME_VAR}`, `$SOME_VAR` to `$env:SOME_VAR`.
#[inline]
#[cfg(target_os = "windows")]
fn to_powershell_variable(input: String) -> String {
    if let Some(var_str) = input.strip_prefix("${") {
        if var_str.find(':').is_none() {
            // If the input starts with "${", remove the trailing "}"
            format!("$env:{}", &var_str[..var_str.len() - 1])
        } else {
            // `${SOME_VAR:-SOME_DEFAULT}`, we currently do not handle this situation,
            // which will result in the task failing to run in such cases.
            input
        }
    } else if let Some(var_str) = input.strip_prefix('$') {
        // If the input starts with "$", directly append to "$env:"
        format!("$env:{}", var_str)
    } else {
        // If no prefix is found, return the input as is
        input
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsShellType {
    Powershell,
    Cmd,
    Other,
}
