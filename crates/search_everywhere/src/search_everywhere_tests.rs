use super::*;
use editor::Editor;
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::{TestAppContext, VisualTestContext};
use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
use serde_json::json;
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};
use util::path;
use workspace::{AppState, MultiWorkspace};

gpui::actions!(search_everywhere_test, [RecordedAction]);

type SymbolRequest = (String, oneshot::Sender<Vec<lsp::SymbolInformation>>);

fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        menu::init();
        workspace::init(app_state.clone(), cx);
        command_palette::init(cx);
        init(cx);
        app_state
    })
}

struct TestWorkspace<'a> {
    workspace: Entity<Workspace>,
    editor: Entity<Editor>,
    action_count: Arc<AtomicUsize>,
    cx: &'a mut VisualTestContext,
}

/// A workspace over `/dir` with a focused editor (so the command palette
/// source has actions to list) and a test action that counts invocations.
async fn build_workspace<'a>(
    project: Entity<Project>,
    cx: &'a mut TestAppContext,
) -> TestWorkspace<'a> {
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspace().clone()
    });
    let action_count = Arc::new(AtomicUsize::new(0));
    let editor = cx.new_window_entity(|window, cx| Editor::single_line(window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let action_count = action_count.clone();
        workspace.register_action(move |_, _: &RecordedAction, _, _| {
            action_count.fetch_add(1, Ordering::SeqCst);
        });
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx));
    });
    cx.run_until_parked();
    TestWorkspace {
        workspace,
        editor,
        action_count,
        cx,
    }
}

fn open_search_everywhere(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<SearchEverywhereDelegate>> {
    cx.dispatch_action(Toggle);
    cx.run_until_parked();
    workspace.update(cx, |workspace, cx| {
        workspace
            .active_modal::<SearchEverywhere>(cx)
            .expect("search everywhere should be open")
            .read(cx)
            .picker()
            .clone()
    })
}

fn set_query(
    picker: &Entity<Picker<SearchEverywhereDelegate>>,
    query: &str,
    cx: &mut VisualTestContext,
) {
    picker.update_in(cx, |picker, window, cx| picker.set_query(query, window, cx));
}

fn rows(
    picker: &Entity<Picker<SearchEverywhereDelegate>>,
    cx: &mut VisualTestContext,
) -> Vec<Row> {
    picker.read_with(cx, |picker, _| picker.delegate.rows().to_vec())
}

fn select_row(
    picker: &Entity<Picker<SearchEverywhereDelegate>>,
    row: Row,
    cx: &mut VisualTestContext,
) {
    picker.update_in(cx, |picker, window, cx| {
        let ix = picker
            .delegate
            .rows()
            .iter()
            .position(|candidate| *candidate == row)
            .expect("row should be listed");
        picker.set_selected_index(ix, None, false, window, cx);
    });
}

fn is_modal_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update(cx, |workspace, cx| {
        workspace.active_modal::<SearchEverywhere>(cx).is_some()
    })
}

fn active_path(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.update(cx, |workspace, cx| {
        let item = workspace.active_item(cx)?;
        let project_path = item.project_path(cx)?;
        Some(project_path.path.as_unix_str().to_string())
    })
}

/// Registers a fake language server whose workspace/symbol replies are held
/// until the test answers them, so tests control when (and in which order)
/// symbols arrive.
async fn start_symbol_server(
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> (
    (Entity<language::Buffer>, project::lsp_store::OpenLspBufferHandle),
    mpsc::UnboundedReceiver<SymbolRequest>,
) {
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(Arc::new(Language::new(
        LanguageConfig {
            name: "Rust".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        None,
    )));
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
        })
        .await
        .unwrap();

    let (requests_tx, requests_rx) = mpsc::unbounded();
    let fake_server = fake_servers.next().await.unwrap();
    fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(move |params, _| {
        let (reply_tx, reply_rx) = oneshot::channel();
        requests_tx
            .unbounded_send((params.query, reply_tx))
            .unwrap();
        async move {
            let symbols = reply_rx.await.unwrap_or_default();
            Ok(Some(lsp::WorkspaceSymbolResponse::Flat(symbols)))
        }
    });
    (buffer, requests_rx)
}

fn symbol(name: &str, kind: lsp::SymbolKind) -> lsp::SymbolInformation {
    #[allow(deprecated)]
    lsp::SymbolInformation {
        name: name.to_string(),
        kind,
        tags: None,
        deprecated: None,
        container_name: None,
        location: lsp::Location::new(
            lsp::Uri::from_file_path(Path::new(path!("/dir/main.rs"))).unwrap(),
            lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 0)),
        ),
    }
}

fn pending_requests(requests: &mut mpsc::UnboundedReceiver<SymbolRequest>) -> Vec<SymbolRequest> {
    let mut pending = Vec::new();
    while let Ok(request) = requests.try_recv() {
        pending.push(request);
    }
    pending
}

#[gpui::test]
async fn test_grouping_and_confirm_action(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/dir"),
            json!({
                "recorded_notes.md": "",
                "notes_1.txt": "",
                "notes_2.txt": "",
                "notes_3.txt": "",
                "notes_4.txt": "",
                "notes_5.txt": "",
                "notes_6.txt": "",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/dir").as_ref()], cx).await;
    let TestWorkspace {
        workspace,
        action_count,
        cx,
        ..
    } = build_workspace(project, cx).await;

    let picker = open_search_everywhere(&workspace, cx);
    set_query(&picker, "recorded", cx);
    cx.run_until_parked();

    // The file finder's own "create new file" row stays part of its section.
    let listed = rows(&picker, cx);
    let actions_header = listed
        .iter()
        .position(|row| *row == Row::Header(Category::Actions))
        .expect("actions section");
    assert_eq!(listed[0], Row::Header(Category::Files));
    assert!(
        listed[1..actions_header]
            .iter()
            .all(|row| matches!(row, Row::Match(Source::Files, _))),
        "only files under the files header: {listed:?}"
    );
    assert_eq!(listed[actions_header + 1], Row::Match(Source::Actions, 0));
    assert!(!listed.contains(&Row::Header(Category::Symbols)));
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.selected_index(), 1, "first match selected");
    });

    // Sections cap at SECTION_LIMIT and offer the rest behind "Show all". The
    // eighth file row is the file finder's "create new file" entry.
    set_query(&picker, "notes", cx);
    cx.run_until_parked();
    let listed = rows(&picker, cx);
    let mut expected = vec![Row::Header(Category::Files)];
    expected.extend((0..SECTION_LIMIT).map(|ix| Row::Match(Source::Files, ix)));
    expected.push(Row::ShowAll(Category::Files, 8));
    assert_eq!(&listed[..expected.len()], &expected[..]);
    assert!(
        listed[expected.len()..]
            .iter()
            .all(|row| !matches!(row, Row::Match(Source::Files, _))),
        "no file rows outside the files section: {listed:?}"
    );

    // Confirming "Show all" switches to that category without dismissing.
    select_row(&picker, Row::ShowAll(Category::Files, 8), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.category(), Category::Files);
        assert_eq!(
            picker.delegate.rows(),
            (0..8)
                .map(|ix| Row::Match(Source::Files, ix))
                .collect::<Vec<_>>()
        );
    });
    assert!(is_modal_open(&workspace, cx));

    // Tab cycles categories: Files -> Symbols -> Actions.
    cx.dispatch_action(picker::ConfirmCompletion);
    cx.dispatch_action(picker::ConfirmCompletion);
    cx.run_until_parked();
    set_query(&picker, "recorded", cx);
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.category(), Category::Actions);
        assert!(
            picker
                .delegate
                .rows()
                .iter()
                .all(|row| matches!(row, Row::Match(Source::Actions, _)))
        );
    });

    select_row(&picker, Row::Match(Source::Actions, 0), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    assert_eq!(action_count.load(Ordering::SeqCst), 1);
    assert!(!is_modal_open(&workspace, cx));
}

#[gpui::test]
async fn test_confirm_file(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/dir"), json!({ "first.txt": "", "second.txt": "" }))
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/dir").as_ref()], cx).await;
    let TestWorkspace { workspace, cx, .. } = build_workspace(project, cx).await;

    let picker = open_search_everywhere(&workspace, cx);
    set_query(&picker, "second", cx);
    cx.run_until_parked();
    select_row(&picker, Row::Match(Source::Files, 0), cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert!(!is_modal_open(&workspace, cx));
    assert_eq!(active_path(&workspace, cx).as_deref(), Some("second.txt"));
}

#[gpui::test]
async fn test_symbols_arrive_after_files_and_actions(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/dir"),
            json!({ "main.rs": "\nfn recorded() {}\nstruct RecordedEvent;\n", "recorded.txt": "" }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/dir").as_ref()], cx).await;
    let (_buffer, mut requests) = start_symbol_server(&project, cx).await;
    let TestWorkspace {
        workspace,
        editor,
        cx,
        ..
    } = build_workspace(project, cx).await;

    let picker = open_search_everywhere(&workspace, cx);
    set_query(&picker, "recorded", cx);
    cx.run_until_parked();

    // The language server hasn't answered, and that holds back nothing else.
    let listed = rows(&picker, cx);
    assert!(listed.contains(&Row::Match(Source::Files, 0)));
    assert!(listed.contains(&Row::Match(Source::Actions, 0)));
    assert!(!listed.contains(&Row::Header(Category::Symbols)));

    // Move the selection onto an action; the late symbols must not steal it.
    select_row(&picker, Row::Match(Source::Actions, 0), cx);

    // The symbol picker's construction-time empty query reaches the server
    // too, but its task is already gone.
    let mut pending = pending_requests(&mut requests);
    assert_eq!(
        pending
            .iter()
            .map(|(query, _)| query.as_str())
            .collect::<Vec<_>>(),
        vec!["", "recorded"]
    );
    let (_, reply) = pending.pop().unwrap();
    reply
        .send(vec![
            symbol("recorded", lsp::SymbolKind::FUNCTION),
            symbol("RecordedEvent", lsp::SymbolKind::STRUCT),
        ])
        .unwrap();
    cx.run_until_parked();

    let listed = rows(&picker, cx);
    let section_order = listed
        .iter()
        .filter_map(|row| match row {
            Row::Header(category) => Some(*category),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        section_order,
        vec![Category::Files, Category::Symbols, Category::Actions]
    );
    picker.read_with(cx, |picker, _| {
        let selected = picker.delegate.rows()[picker.delegate.selected_index()];
        assert_eq!(selected, Row::Match(Source::Actions, 0));
    });

    // Types come out of the same symbol results, filtered by kind.
    picker.update_in(cx, |picker, window, cx| {
        SearchEverywhereDelegate::set_category(picker, Category::Types, window, cx);
    });
    picker.read_with(cx, |picker, cx| {
        let rows = picker.delegate.rows();
        assert_eq!(rows.len(), 1);
        let Row::Match(Source::Symbols, ix) = rows[0] else {
            panic!("expected a symbol row, got {rows:?}");
        };
        let symbols = &picker.delegate.symbols.as_ref().unwrap().read(cx).delegate;
        assert_eq!(symbols.symbol_for_match(ix).unwrap().name, "RecordedEvent");
    });

    // Confirming a symbol opens its file at the symbol.
    picker.update_in(cx, |picker, window, cx| {
        SearchEverywhereDelegate::set_category(picker, Category::All, window, cx);
    });
    let recorded_row = picker.read_with(cx, |picker, cx| {
        let symbols = &picker.delegate.symbols.as_ref().unwrap().read(cx).delegate;
        picker
            .delegate
            .rows()
            .iter()
            .copied()
            .find(|row| match row {
                Row::Match(Source::Symbols, ix) => symbols
                    .symbol_for_match(*ix)
                    .is_some_and(|symbol| symbol.name == "recorded"),
                _ => false,
            })
            .unwrap()
    });
    select_row(&picker, recorded_row, cx);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();

    assert!(!is_modal_open(&workspace, cx));
    assert_eq!(active_path(&workspace, cx).as_deref(), Some("main.rs"));
    let opened = workspace.update(cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx).unwrap()
    });
    assert_ne!(opened, editor);
    opened.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        let cursor = editor.selections.newest::<language::Point>(&snapshot).head();
        assert_eq!(cursor.row, 1, "cursor should be on the symbol's line");
    });
}

#[gpui::test]
async fn test_stale_symbol_replies_are_dropped(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/dir"), json!({ "main.rs": "\nfn one() {}\n" }))
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/dir").as_ref()], cx).await;
    let (_buffer, mut requests) = start_symbol_server(&project, cx).await;
    let TestWorkspace { workspace, cx, .. } = build_workspace(project, cx).await;

    let picker = open_search_everywhere(&workspace, cx);
    set_query(&picker, "on", cx);
    cx.run_until_parked();
    set_query(&picker, "one", cx);
    cx.run_until_parked();

    // Both requests reached the server. Answer the current one first, then
    // the stale one with more (wrong) results, as a slow server would.
    let mut pending = pending_requests(&mut requests);
    assert_eq!(
        pending
            .iter()
            .map(|(query, _)| query.as_str())
            .collect::<Vec<_>>(),
        vec!["", "on", "one"]
    );
    let (_, current_reply) = pending.pop().unwrap();
    let (_, stale_reply) = pending.pop().unwrap();
    current_reply
        .send(vec![symbol("one", lsp::SymbolKind::FUNCTION)])
        .unwrap();
    cx.run_until_parked();
    // The stale request's future was dropped along with its task, so the
    // server may already see a closed channel; either way nothing may land.
    stale_reply
        .send(vec![
            symbol("on_a", lsp::SymbolKind::FUNCTION),
            symbol("on_b", lsp::SymbolKind::FUNCTION),
            symbol("on_c", lsp::SymbolKind::FUNCTION),
        ])
        .ok();
    cx.run_until_parked();

    picker.read_with(cx, |picker, cx| {
        let symbols = &picker.delegate.symbols.as_ref().unwrap().read(cx).delegate;
        let names = picker
            .delegate
            .rows()
            .iter()
            .filter_map(|row| match row {
                Row::Match(Source::Symbols, ix) => symbols
                    .symbol_for_match(*ix)
                    .map(|symbol| symbol.name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["one".to_string()]);
    });
}
