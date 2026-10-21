#[cfg(test)]
mod search_everywhere_tests;

use command_palette::CommandPaletteDelegate;
use file_finder::FileFinderDelegate;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Subscription, Task, WeakEntity, Window, actions,
};
use language::SymbolKind;
use picker::{Picker, PickerDelegate, ScrollBehavior};
use project::Project;
use project_symbols::ProjectSymbolsDelegate;
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, ListSubHeader, TintColor, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Workspace};

actions!(
    search_everywhere,
    [
        /// Searches files, symbols, types and actions from a single picker.
        Toggle,
        /// Shows the next result category in Search Everywhere.
        NextCategory,
        /// Shows the previous result category in Search Everywhere.
        PreviousCategory,
    ]
);

/// How many matches each section shows on the All tab before collapsing the
/// rest behind a "Show all" row.
const SECTION_LIMIT: usize = 5;

pub fn init(cx: &mut App) {
    cx.observe_new(SearchEverywhere::register).detach();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    All,
    Types,
    Files,
    Symbols,
    Actions,
}

impl Category {
    const TABS: [Category; 5] = [
        Category::All,
        Category::Types,
        Category::Files,
        Category::Symbols,
        Category::Actions,
    ];

    fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::Types => "Types",
            Category::Files => "Files",
            Category::Symbols => "Symbols",
            Category::Actions => "Actions",
        }
    }

    fn cycle(self, step: isize) -> Self {
        let index = Self::TABS
            .iter()
            .position(|category| *category == self)
            .unwrap_or(0);
        let next = (index as isize + step).rem_euclid(Self::TABS.len() as isize) as usize;
        Self::TABS[next]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Files,
    Symbols,
    Actions,
}

impl Source {
    fn category(self) -> Category {
        match self {
            Source::Files => Category::Files,
            Source::Symbols => Category::Symbols,
            Source::Actions => Category::Actions,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Header(Category),
    /// A match in one of the inner pickers, by its index in that picker.
    Match(Source, usize),
    ShowAll(Category, usize),
}

pub struct SearchEverywhere {
    picker: Entity<Picker<SearchEverywhereDelegate>>,
}

impl ModalView for SearchEverywhere {}

impl EventEmitter<DismissEvent> for SearchEverywhere {}

impl Focusable for SearchEverywhere {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl SearchEverywhere {
    fn register(workspace: &mut Workspace, _: Option<&mut Window>, _: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, window, cx).detach();
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<()> {
        if workspace.active_modal::<Self>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return Task::ready(());
        }
        let Some(previous_focus_handle) = window.focused(cx) else {
            return Task::ready(());
        };
        let files = FileFinderDelegate::new_detached(workspace, cx);
        cx.spawn_in(window, async move |workspace, cx| {
            let files = files.await;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let weak_workspace = cx.entity().downgrade();
                    let project = workspace.project().clone();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        Self::new(
                            files,
                            weak_workspace,
                            project,
                            previous_focus_handle,
                            window,
                            cx,
                        )
                    });
                })
                .log_err();
        })
    }

    fn new(
        files: FileFinderDelegate,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        previous_focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let actions = CommandPaletteDelegate::for_focused(
            workspace.clone(),
            previous_focus_handle,
            window,
            cx,
        );
        let files = cx.new(|cx| Picker::nonsearchable_list(files, window, cx).reopenable(false, cx));
        let actions =
            cx.new(|cx| Picker::nonsearchable_list(actions, window, cx).reopenable(false, cx));
        let search_everywhere = cx.entity().downgrade();
        let picker = cx.new(|cx| {
            let delegate = SearchEverywhereDelegate {
                _subscriptions: vec![
                    cx.observe_in(&files, window, SearchEverywhereDelegate::source_updated),
                    cx.observe_in(&actions, window, SearchEverywhereDelegate::source_updated),
                    cx.subscribe_in(&project, window, SearchEverywhereDelegate::project_event),
                ],
                search_everywhere,
                workspace,
                project,
                files,
                symbols: None,
                actions,
                category: Category::All,
                query: None,
                rows: Vec::new(),
                selected_index: 0,
            };
            Picker::list(delegate, window, cx)
                .initial_width(rems(42.))
                .show_scrollbar(true)
        });
        Self { picker }
    }

    fn next_category(&mut self, _: &NextCategory, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            let category = picker.delegate.category.cycle(1);
            SearchEverywhereDelegate::set_category(picker, category, window, cx);
        });
    }

    fn previous_category(
        &mut self,
        _: &PreviousCategory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            let category = picker.delegate.category.cycle(-1);
            SearchEverywhereDelegate::set_category(picker, category, window, cx);
        });
    }

    #[cfg(test)]
    fn picker(&self) -> &Entity<Picker<SearchEverywhereDelegate>> {
        &self.picker
    }
}

impl Render for SearchEverywhere {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SearchEverywhere")
            .on_action(cx.listener(Self::next_category))
            .on_action(cx.listener(Self::previous_category))
            .child(self.picker.clone())
    }
}

/// Fans one query out to hidden file finder, project symbols and command
/// palette pickers and lists their matches in sections. Each source keeps its
/// own matching, ranking, cancellation and confirm behavior; this delegate only
/// decides which of their matches to show and where.
pub struct SearchEverywhereDelegate {
    search_everywhere: WeakEntity<SearchEverywhere>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    files: Entity<Picker<FileFinderDelegate>>,
    /// Created on the first non-empty query. A picker runs an empty query when
    /// it's built, and for symbols that is a workspace/symbol request asking
    /// every language server for everything, which most opens never need.
    symbols: Option<Entity<Picker<ProjectSymbolsDelegate>>>,
    actions: Entity<Picker<CommandPaletteDelegate>>,
    category: Category,
    /// The query last sent to the sources. `None` until the first one, so the
    /// initial empty query isn't mistaken for a refresh.
    query: Option<String>,
    rows: Vec<Row>,
    selected_index: usize,
    _subscriptions: Vec<Subscription>,
}

impl SearchEverywhereDelegate {
    fn source_updated<V>(
        picker: &mut Picker<Self>,
        _: Entity<V>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        // Sources finish at different times (language servers can take
        // seconds), so each one refreshes the rows as it lands. Re-running the
        // current query is what resizes the picker's list; `update_matches`
        // sees the same query and only rebuilds rows.
        Self::refresh(picker, ScrollBehavior::PreserveOffset, window, cx);
    }

    fn project_event(
        picker: &mut Picker<Self>,
        _: &Entity<Project>,
        event: &project::Event,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        // The file finder refreshes itself from its own modal, which isn't here.
        match event {
            project::Event::WorktreeUpdatedEntries(..)
            | project::Event::WorktreeAdded(_)
            | project::Event::WorktreeRemoved(_) => {
                let query = picker.delegate.query.clone().unwrap_or_default();
                picker.delegate.files.update(cx, |files, cx| {
                    files.update_matches(query, window, cx);
                });
            }
            _ => {}
        }
    }

    fn set_category(
        picker: &mut Picker<Self>,
        category: Category,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        picker.delegate.category = category;
        Self::refresh(picker, ScrollBehavior::RevealSelected, window, cx);
    }

    /// For category changes made from inside the delegate, which can't reach
    /// the picker to resize its list.
    fn refresh_deferred(window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.defer_in(window, |picker, window, cx| {
            Self::refresh(picker, ScrollBehavior::RevealSelected, window, cx);
        });
    }

    fn refresh(
        picker: &mut Picker<Self>,
        scroll_behavior: ScrollBehavior,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if let Some(query) = picker.delegate.query.clone() {
            picker.update_matches_with_options(query, scroll_behavior, window, cx);
        }
    }

    fn symbols_source(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Entity<Picker<ProjectSymbolsDelegate>> {
        if let Some(symbols) = &self.symbols {
            return symbols.clone();
        }
        let delegate = ProjectSymbolsDelegate::new(self.workspace.clone(), self.project.clone());
        let symbols =
            cx.new(|cx| Picker::nonsearchable_list(delegate, window, cx).reopenable(false, cx));
        self._subscriptions
            .push(cx.observe_in(&symbols, window, Self::source_updated));
        self.symbols = Some(symbols.clone());
        symbols
    }

    pub fn category(&self) -> Category {
        self.category
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn has_query(&self) -> bool {
        self.query
            .as_deref()
            .is_some_and(|query| !query.trim().is_empty())
    }

    fn source_matches(&self, source: Source, types_only: bool, cx: &App) -> Vec<usize> {
        match source {
            Source::Files => (0..self.files.read(cx).delegate.match_count()).collect(),
            // An empty workspace/symbol request asks every language server for
            // everything, which isn't useful in a mixed list.
            Source::Symbols if !self.has_query() => Vec::new(),
            Source::Symbols => {
                let Some(symbols) = &self.symbols else {
                    return Vec::new();
                };
                let symbols = &symbols.read(cx).delegate;
                (0..symbols.match_count())
                    .filter(|ix| {
                        !types_only
                            || symbols
                                .symbol_for_match(*ix)
                                .is_some_and(|symbol| is_type(symbol.kind))
                    })
                    .collect()
            }
            Source::Actions => (0..self.actions.read(cx).delegate.match_count()).collect(),
        }
    }

    fn rebuild_rows(&mut self, keep_selection: bool, cx: &App) {
        let previous_row = self
            .rows
            .get(self.selected_index)
            .copied()
            .filter(|_| keep_selection);
        let was_on_first_row = Some(self.selected_index) == self.first_selectable_index();

        let mut rows = Vec::new();
        match self.category {
            Category::All => {
                for source in [Source::Files, Source::Symbols, Source::Actions] {
                    let matches = self.source_matches(source, false, cx);
                    if matches.is_empty() {
                        continue;
                    }
                    rows.push(Row::Header(source.category()));
                    rows.extend(
                        matches
                            .iter()
                            .take(SECTION_LIMIT)
                            .map(|ix| Row::Match(source, *ix)),
                    );
                    if matches.len() > SECTION_LIMIT {
                        rows.push(Row::ShowAll(source.category(), matches.len()));
                    }
                }
            }
            Category::Types => rows.extend(
                self.source_matches(Source::Symbols, true, cx)
                    .into_iter()
                    .map(|ix| Row::Match(Source::Symbols, ix)),
            ),
            Category::Files => rows.extend(
                self.source_matches(Source::Files, false, cx)
                    .into_iter()
                    .map(|ix| Row::Match(Source::Files, ix)),
            ),
            Category::Symbols => rows.extend(
                self.source_matches(Source::Symbols, false, cx)
                    .into_iter()
                    .map(|ix| Row::Match(Source::Symbols, ix)),
            ),
            Category::Actions => rows.extend(
                self.source_matches(Source::Actions, false, cx)
                    .into_iter()
                    .map(|ix| Row::Match(Source::Actions, ix)),
            ),
        }
        self.rows = rows;

        // A late source (usually symbols) inserts rows above the selection.
        // Follow the selected item if the user moved to it; otherwise stay on
        // the first row, which is where Enter is expected to land.
        let kept_index = previous_row
            .filter(|_| !was_on_first_row)
            .and_then(|row| self.rows.iter().position(|candidate| *candidate == row));
        self.selected_index = kept_index
            .or_else(|| self.first_selectable_index())
            .unwrap_or(0);
    }

    fn first_selectable_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| !matches!(row, Row::Header(_)))
    }
}

fn is_type(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum
    )
}

fn render_source_match<D: PickerDelegate>(
    source: &Entity<Picker<D>>,
    ix: usize,
    selected: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    source.update(cx, |picker, cx| {
        picker
            .delegate
            .render_match(ix, selected, window, cx)
            .map(IntoElement::into_any_element)
    })
}

fn confirm_source_match<D: PickerDelegate>(
    source: &Entity<Picker<D>>,
    ix: usize,
    secondary: bool,
    window: &mut Window,
    cx: &mut App,
) {
    source.update(cx, |picker, cx| {
        picker.delegate.set_selected_index(ix, window, cx);
        picker.delegate.confirm(secondary, window, cx);
    });
}

impl PickerDelegate for SearchEverywhereDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "search everywhere"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search files, symbols and actions...".into()
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        !matches!(self.rows.get(ix), Some(Row::Header(_)) | None)
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if self.query.as_ref() != Some(&query) {
            // Replacing each source's in-flight query drops its pending task,
            // which is how stale file searches and language server replies get
            // cancelled.
            self.files.update(cx, |files, cx| {
                files.update_matches(query.clone(), window, cx);
            });
            self.actions.update(cx, |actions, cx| {
                actions.update_matches(query.clone(), window, cx);
            });
            if !query.trim().is_empty() {
                self.symbols_source(window, cx).update(cx, |symbols, cx| {
                    symbols.update_matches(query.clone(), window, cx);
                });
            }
            self.query = Some(query);
            self.rebuild_rows(false, cx);
        } else {
            self.rebuild_rows(true, cx);
        }
        Task::ready(())
    }

    fn finalize_update_matches(
        &mut self,
        query: String,
        duration: std::time::Duration,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        // Matching actions is in-memory and fast, so give it the same short
        // synchronous window the command palette gets. That keeps Enter right
        // after typing from confirming the previous query's action.
        self.actions.update(cx, |actions, cx| {
            actions
                .delegate
                .finalize_update_matches(query, duration, window, cx)
        });
        self.rebuild_rows(true, cx);
        true
    }

    fn confirm_completion(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        // Tab is bound to `ConfirmCompletion` in pickers. Returning the query
        // unchanged keeps the action from falling through to the editor (which
        // would insert a tab) and makes the picker refresh into the new tab.
        self.category = self.category.cycle(1);
        Self::refresh_deferred(window, cx);
        Some(query)
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.rows.get(self.selected_index).copied() {
            Some(Row::ShowAll(category, _)) => {
                self.category = category;
                Self::refresh_deferred(window, cx);
            }
            Some(Row::Match(source, ix)) => {
                // Dismiss first, as the command palette does, so an action that
                // opens its own modal doesn't find this one still in the way.
                self.search_everywhere
                    .update(cx, |_, cx| cx.emit(DismissEvent))
                    .ok();
                match source {
                    Source::Files => confirm_source_match(&self.files, ix, secondary, window, cx),
                    Source::Symbols => {
                        if let Some(symbols) = &self.symbols {
                            confirm_source_match(symbols, ix, secondary, window, cx)
                        }
                    }
                    Source::Actions => {
                        confirm_source_match(&self.actions, ix, secondary, window, cx)
                    }
                }
            }
            Some(Row::Header(_)) | None => {}
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.search_everywhere
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_header(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            h_flex()
                .px_2()
                .py_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .children(Category::TABS.into_iter().map(|category| {
                    Button::new(category.label(), category.label())
                        .label_size(LabelSize::Small)
                        .toggle_state(category == self.category)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(cx.listener(move |picker, _, window, cx| {
                            Self::set_category(picker, category, window, cx);
                        }))
                }))
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match *self.rows.get(ix)? {
            Row::Header(category) => Some(
                ListSubHeader::new(category.label())
                    .inset(true)
                    .into_any_element(),
            ),
            Row::Match(Source::Files, ix) => {
                render_source_match(&self.files, ix, selected, window, cx)
            }
            Row::Match(Source::Symbols, ix) => {
                render_source_match(self.symbols.as_ref()?, ix, selected, window, cx)
            }
            Row::Match(Source::Actions, ix) => {
                render_source_match(&self.actions, ix, selected, window, cx)
            }
            Row::ShowAll(category, count) => Some(
                ListItem::new(("show-all", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .child(
                        Label::new(format!(
                            "Show all {count} {}",
                            category.label().to_lowercase()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
        }
    }
}
