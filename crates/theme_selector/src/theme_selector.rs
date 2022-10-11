use fuzzy::{match_strings, StringMatch, StringMatchCandidate};
use gpui::{
    actions, anyhow::Result, elements::*, AnyViewHandle, AppContext, Element, ElementBox, Entity,
    MouseState, MutableAppContext, RenderContext, View, ViewContext, ViewHandle,
};
use paths::SETTINGS;
use picker::{Picker, PickerDelegate};
use settings::{write_theme, Settings};
use smol::{fs::read_to_string, io::AsyncWriteExt};
use std::sync::Arc;
use theme::{Theme, ThemeMeta, ThemeRegistry};
use workspace::{AppState, Workspace};

pub struct ThemeSelector {
    registry: Arc<ThemeRegistry>,
    theme_data: Vec<ThemeMeta>,
    matches: Vec<StringMatch>,
    original_theme: Arc<Theme>,
    picker: ViewHandle<Picker<Self>>,
    selection_completed: bool,
    selected_index: usize,
}

actions!(theme_selector, [Toggle, Reload]);

pub fn init(app_state: Arc<AppState>, cx: &mut MutableAppContext) {
    Picker::<ThemeSelector>::init(cx);
    cx.add_action({
        let theme_registry = app_state.themes.clone();
        move |workspace, _: &Toggle, cx| {
            ThemeSelector::toggle(workspace, theme_registry.clone(), cx)
        }
    });
}

pub enum Event {
    Dismissed,
}

impl ThemeSelector {
    fn new(registry: Arc<ThemeRegistry>, cx: &mut ViewContext<Self>) -> Self {
        let handle = cx.weak_handle();
        let picker = cx.add_view(|cx| Picker::new(handle, cx));
        let settings = cx.global::<Settings>();

        let original_theme = settings.theme.clone();

        let mut theme_names = registry
            .list(
                settings.staff_mode,
                settings.experiments.experimental_themes,
            )
            .collect::<Vec<_>>();
        theme_names.sort_unstable_by(|a, b| {
            a.is_light
                .cmp(&b.is_light)
                .reverse()
                .then(a.name.cmp(&b.name))
        });
        let matches = theme_names
            .iter()
            .map(|meta| StringMatch {
                candidate_id: 0,
                score: 0.0,
                positions: Default::default(),
                string: meta.name.clone(),
            })
            .collect();
        let mut this = Self {
            registry,
            theme_data: theme_names,
            matches,
            picker,
            original_theme: original_theme.clone(),
            selected_index: 0,
            selection_completed: false,
        };
        this.select_if_matching(&original_theme.meta.name);
        this
    }

    fn toggle(
        workspace: &mut Workspace,
        themes: Arc<ThemeRegistry>,
        cx: &mut ViewContext<Workspace>,
    ) {
        workspace.toggle_modal(cx, |_, cx| {
            let this = cx.add_view(|cx| Self::new(themes, cx));
            cx.subscribe(&this, Self::on_event).detach();
            this
        });
    }

    #[cfg(debug_assertions)]
    pub fn reload(themes: Arc<ThemeRegistry>, cx: &mut MutableAppContext) {
        let current_theme_name = cx.global::<Settings>().theme.meta.name.clone();
        themes.clear();
        match themes.get(&current_theme_name) {
            Ok(theme) => {
                Self::set_theme(theme, cx);
                log::info!("reloaded theme {}", current_theme_name);
            }
            Err(error) => {
                log::error!("failed to load theme {}: {:?}", current_theme_name, error)
            }
        }
    }

    fn show_selected_theme(&mut self, cx: &mut ViewContext<Self>) {
        if let Some(mat) = self.matches.get(self.selected_index) {
            match self.registry.get(&mat.string) {
                Ok(theme) => {
                    Self::set_theme(theme, cx);

                    let theme_name = mat.string.clone();

                    // cx.global::<SettingsLock()>
                    // cx.global::<SettingsFile>() // lock

                    // 1) Truncation can cause data loss, make it atomic by creating tmp file and moving
                    // 2) Maybe firing too often? Conceptually want to commit
                    // Having a lock on the settings file

                    //    -
                    //   | |
                    // FS _

                    // General problem: whenever we want to persist stuff
                    // In memory representation -> File on disk
                    // Write font size,
                    // Write theme to disk
                    // Write -> See your own write -> Another Write

                    // Memory Write 1 -> Write To Disk 1, | Memory Write 2,
                    //                                    Blocking ->>>>>> | Read From Disk 1,
                    //                                    Discard          | Read WHATEVER is from disk |

                    // Blocking lock ->

                    // Whenever we update the settings in memory, we enqueue a write to disk
                    // When we receive a file system event, we only honor it if all pending disk writes are complete.

                    // When the settings become dirty in memory, schedule a write to disk
                    // When we are sure the write is completed, consider the settings clean
                    // Only read settings from disk into memory when in memory settings are clean
                    // read settings just does not happen, if the settings are dirty

                    // 10 settings queued up:
                    // lock() -> Only needs to be on the file
                    // How to set a setting:
                    // write to memory
                    // Read the whole file from disk
                    // Surgically inject the setting string
                    // Write to disk
                    // unlock()

                    // Write 10 x change font size
                    // Read-open-write font size
                    // Read-open-write font size
                    // Read-open-write font size
                    // Read-open-write font size
                    // Read-open-write font size
                    // ..
                    // Read from file system, only gets the latest font size and uselessly sets font size

                    // `SettingsFile`
                    // You can non-blocking, write to it as much as you need
                    // Debounces your changes, waits for you to be done, and then flushes them all to the file system
                    // And blocks the read

                    // Read and write to memory. ^ up from the file system

                    // If there's pendings writes, we need to wait until this whole thing is done'

                    cx.background()
                        .spawn(async move {
                            match write_theme_name(theme_name).await {
                                Ok(_) => {}
                                Err(_) => return, //TODO Pop toast
                            }
                        })
                        .detach()
                }
                Err(error) => {
                    log::error!("error loading theme {}: {}", mat.string, error)
                }
            }
        }
    }

    fn select_if_matching(&mut self, theme_name: &str) {
        self.selected_index = self
            .matches
            .iter()
            .position(|mat| mat.string == theme_name)
            .unwrap_or(self.selected_index);
    }

    fn on_event(
        workspace: &mut Workspace,
        _: ViewHandle<ThemeSelector>,
        event: &Event,
        cx: &mut ViewContext<Workspace>,
    ) {
        match event {
            Event::Dismissed => {
                workspace.dismiss_modal(cx);
            }
        }
    }

    fn set_theme(theme: Arc<Theme>, cx: &mut MutableAppContext) {
        cx.update_global::<Settings, _, _>(|settings, cx| {
            settings.theme = theme;
            cx.refresh_windows();
        });
    }
}

impl PickerDelegate for ThemeSelector {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn confirm(&mut self, cx: &mut ViewContext<Self>) {
        self.selection_completed = true;
        cx.emit(Event::Dismissed);
    }

    fn dismiss(&mut self, cx: &mut ViewContext<Self>) {
        if !self.selection_completed {
            Self::set_theme(self.original_theme.clone(), cx);
            self.selection_completed = true;
        }
        cx.emit(Event::Dismissed);
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut ViewContext<Self>) {
        self.selected_index = ix;
        self.show_selected_theme(cx);
    }

    fn update_matches(&mut self, query: String, cx: &mut ViewContext<Self>) -> gpui::Task<()> {
        let background = cx.background().clone();
        let candidates = self
            .theme_data
            .iter()
            .enumerate()
            .map(|(id, meta)| StringMatchCandidate {
                id,
                char_bag: meta.name.as_str().into(),
                string: meta.name.clone(),
            })
            .collect::<Vec<_>>();

        cx.spawn(|this, mut cx| async move {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(&mut cx, |this, cx| {
                this.matches = matches;
                this.selected_index = this
                    .selected_index
                    .min(this.matches.len().saturating_sub(1));
                this.show_selected_theme(cx);
                cx.notify();
            });
        })
    }

    fn render_match(
        &self,
        ix: usize,
        mouse_state: MouseState,
        selected: bool,
        cx: &AppContext,
    ) -> ElementBox {
        let settings = cx.global::<Settings>();
        let theme = &settings.theme;
        let theme_match = &self.matches[ix];
        let style = theme.picker.item.style_for(mouse_state, selected);

        Label::new(theme_match.string.clone(), style.label.clone())
            .with_highlights(theme_match.positions.clone())
            .contained()
            .with_style(style.container)
            .boxed()
    }
}

impl Entity for ThemeSelector {
    type Event = Event;

    fn release(&mut self, cx: &mut MutableAppContext) {
        if !self.selection_completed {
            Self::set_theme(self.original_theme.clone(), cx);
        }
    }
}

impl View for ThemeSelector {
    fn ui_name() -> &'static str {
        "ThemeSelector"
    }

    fn render(&mut self, _: &mut RenderContext<Self>) -> ElementBox {
        ChildView::new(self.picker.clone()).boxed()
    }

    fn on_focus_in(&mut self, _: AnyViewHandle, cx: &mut ViewContext<Self>) {
        if cx.is_self_focused() {
            cx.focus(&self.picker);
        }
    }
}

async fn write_theme_name(theme_name: String) -> Result<()> {
    let mut settings = read_to_string(SETTINGS.as_path()).await?;
    settings = write_theme(settings, &theme_name);

    let mut file = smol::fs::OpenOptions::new()
        .truncate(true)
        .write(true)
        .open(SETTINGS.as_path())
        .await?;

    file.write_all(settings.as_bytes()).await?;

    Ok(())
}
