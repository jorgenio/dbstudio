//! jorgenio dbstudio: three-pane desktop client. The navigator holds saved
//! connections plus a lazy object tree backed by a live driver session; the
//! editor runs statements through `dbstudio-drivers` and the result grid is
//! rebuilt from the driver's normalized `QueryResult`.

// Ship without a console window; debug builds keep it for panics/logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod designer;
mod er;
mod export;
mod form;
mod grid;
mod icons;
mod store;
mod sync;
mod theme;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, ConnectionId, Engine, ExecuteRequest, ObjectKind,
    ObjectNode, ObjectRef, QueryResult,
};
use dbstudio_drivers::{self as drivers, Session};
use form::{ConnForm, FormMsg};
use store::Connections;
use iced::widget::{
    button, column, container, row, rule, scrollable, stack, text, text_input, Column, MouseArea,
    Space,
};
use iced::{Element, Length, Subscription, Task, Theme};
use iced_autocomplete::{auto_complete, Filter, State as AutocompleteState};
use iced_code_editor::{CodeEditor, Message as EditorMessage};
use oriel_iced::grid::VirtualGrid;

const SAMPLE_SQL: &str = r#"SELECT u.id, u.nombre, u.email, p.total, p.estado
FROM usuarios u
JOIN pedidos p ON p.usuario_id = u.id
WHERE u.activo = 1
ORDER BY p.total DESC
LIMIT 500;
"#;

const MAX_ROWS: usize = 10_000;

#[derive(Clone, Debug)]
enum ConnState {
    /// Saved but never (re)connected in this session.
    Idle,
    Connecting,
    Connected,
    Error(String),
}

struct ConnEntry {
    cfg: ConnectionConfig,
    state: ConnState,
}

/// One query workspace in the tab strip: its own SQL text, result set and
/// connection binding. The live `App` fields mirror `tabs[active_tab]`
/// (stash on switch, load on switch back).
struct Tab {
    title: String,
    content: String,
    connection: Option<ConnectionId>,
    grid: VirtualGrid<grid::ResultStore>,
    executed: bool,
    last_row_count: usize,
    last_columns: Vec<ColumnMeta>,
    query_context: Option<String>,
}

impl Tab {
    fn new(title: String, connection: Option<ConnectionId>) -> Self {
        Self {
            title,
            content: String::new(),
            connection,
            grid: grid::empty(),
            executed: false,
            last_row_count: 0,
            last_columns: Vec::new(),
            query_context: None,
        }
    }
}

/// Workspace-level ER diagram overlay (global: not one per query tab).
struct ErState {
    connection: ConnectionId,
    /// Ready diagram once the batch finished; `None` while loading or empty.
    diagram: Option<er::Diagram>,
    loading: bool,
    /// Per-table failures / notes (e.g. truncated batch).
    failed: Vec<String>,
}

struct App {
    rt: Arc<tokio::runtime::Runtime>,
    store: Connections,
    editor: CodeEditor,
    grid: VirtualGrid<grid::ResultStore>,
    search: AutocompleteState<String>,
    search_text: String,
    entries: Vec<ConnEntry>,
    selected: Option<usize>,
    form: Option<ConnForm>,
    /// Live session per connection (multi-DB: tabs may bind different ones).
    sessions: HashMap<ConnectionId, Arc<Session>>,
    active_id: Option<ConnectionId>,
    /// Query workspaces; the live fields mirror `tabs[active_tab]`.
    tabs: Vec<Tab>,
    active_tab: usize,
    /// Monotonic counter for unique tab titles ("Consulta N").
    next_tab: usize,
    /// Open ER diagram overlay, when the workspace shows it.
    er: Option<ErState>,
    /// Open table designer overlay (mutually exclusive with `er`).
    designer: Option<designer::DesignerState>,
    /// Connection whose `describe_table` is in flight for the designer.
    design_pending: Option<ConnectionId>,
    /// Structure-sync overlay and the destination whose describe is in
    /// flight (stale batches are discarded by matching this).
    sync: Option<SyncState>,
    sync_pending: Option<ConnectionId>,
    /// Navigator filter: narrows the tree to labels containing this text.
    tree_filter: String,
    /// "Mis conexiones" root row expanded (its own toggle, kept out of
    /// `expanded` because it is not an `ObjectRef`).
    expanded_root: bool,
    /// Right-click menu over a navigator row (connection or object node).
    context_menu: Option<ContextMenu>,
    /// Last object opened from the tree: seeds the designer's target.
    selected_object: Option<(ObjectRef, ObjectKind)>,
    root: Option<ObjectRef>,
    tree: HashMap<ObjectRef, Vec<ObjectNode>>,
    expanded: HashSet<ObjectRef>,
    /// Columns of the last executed result set (for "copy with names").
    last_columns: Vec<ColumnMeta>,
    /// Displayed row count of the last result set.
    last_row_count: usize,
    /// Whether a statement has completed at least once this session (drives
    /// the "Resultado 1" tab's active/dimmed state).
    executed: bool,
    /// Driver context for bare MongoDB filters ("db.collection"), set when a
    /// collection node is opened in the editor.
    query_context: Option<String>,
    /// SQL completion popup state (prefix being typed + filtered candidates).
    completion: Completion,
    /// Cached table/column dictionaries per connection (populated eagerly in
    /// the background after each connect).
    completion_words: HashMap<ConnectionId, Vec<String>>,
    /// Dark chrome active? (persisted in settings.json).
    dark: bool,
    /// Entry index being edited in the connection form (None = creating new).
    editing: Option<usize>,
    status: String,
    /// Id of the main window; set once the window is open (the frameless
    /// title bar needs it to drag/minimize/close the window).
    window_id: Option<iced::window::Id>,
    /// When the title bar drag region was last pressed (double-click).
    last_title_press: Option<std::time::Instant>,
}

#[derive(Clone)]
enum Message {
    Editor(EditorMessage),
    Grid(grid::GridEvent),
    SearchInput(String),
    SearchSubmit(String),
    SearchSelect(String),
    SelectConnection(usize),
    /// Remove the selected connection from the sidebar and the store.
    DeleteSelected,
    Run,
    OpenForm,
    Form(FormMsg),
    /// Ok: id, session, root and children. Err: (id, error) so concurrent
    /// handshakes can be attributed to the right entry.
    Connected(Result<ConnectedPayload, (ConnectionId, String)>),
    ChildrenLoaded(ObjectRef, Result<Vec<ObjectNode>, String>),
    TreeToggle(ObjectRef, ObjectKind),
    /// Select a leaf node without loading children (columns and friends).
    TreeSelect(ObjectRef, ObjectKind),
    /// Connection row inside the unified tree: select + expand (or
    /// collapse when it is the active, already-expanded one).
    TreeToggleConn(ConnectionId),
    /// Toggle the synthetic "Mis connections" root row.
    TreeRootToggle,
    /// Navigator filter box.
    TreeFilter(String),
    /// Table/view/collection clicked: put a starter query in the editor and
    /// expand the node's children.
    TreeOpenObject(ObjectRef, ObjectKind),
    /// Statement finished for the tab that started it (`usize` tab index).
    Executed(usize, Result<QueryResult, String>),
    /// Tab strip: open / close / focus.
    NewTab,
    CloseTab(usize),
    SelectTab(usize),
    /// ER diagram: load (or reload), close, and the table batch.
    ErOpen,
    ErClose,
    ErLoaded(Result<er::ErPayload, String>),
    /// Table designer: open new / open from selected table, grid edits,
    /// apply the previewed DDL, close.
    DesignNew,
    DesignSelected,
    DesignerLoaded(Result<DesignerPayload, String>),
    Design(designer::DesignMsg),
    DesignApply,
    DesignApplied(Result<usize, String>),
    DesignClose,
    /// Structure sync: open against the selected table, pick the
    /// destination connection, load both sides, apply, close.
    SyncOpen,
    SyncDest(ConnectionId),
    SyncLoaded(Result<SyncPayload, String>),
    SyncApply,
    SyncApplied(Result<usize, String>),
    SyncClose,
    /// Copy selected rows (all when nothing is selected) as TSV.
    CopyValues,
    /// Copy rows as TSV with a header line of column names.
    CopyWithHeaders,
    /// Open the save dialog for the current result set.
    Export,
    /// The save dialog resolved (None = cancelled).
    ExportTo(Option<std::path::PathBuf>),
    /// The main window reported its id (needed by the custom title bar).
    WindowOpened(iced::window::Id),
    /// Press on the title bar drag region: start an OS window drag (a
    /// second press inside the double-click interval maximizes instead).
    WinDrag,
    /// Title-bar buttons: minimize / maximize / close / system menu.
    WinMinimize,
    WinToggleMax,
    WinClose,
    WinSystemMenu,
    /// Window events (used to re-assert the frameless style after
    /// resizes/focus changes, when winit may re-apply frame styles).
    WinEvent(iced::window::Event),
    /// Press on an invisible edge/corner resize zone.
    WinResize(iced::window::Direction),
    /// Startup frame fix step: 1 = enlarge 1px, 0 = restore 1px.
    WinNudgeTitle(bool),
    /// `iced::window::run` completion marker (no-op).
    WinFrameChecked,
    /// A table/column dictionary finished loading in the background; `id` is
    /// the connection it belongs to (may no longer be the active one).
    CompletionWordsLoaded(ConnectionId, Vec<String>),
    /// A suggestion row in the completion popup was clicked.
    CompletionAccept(usize),
    /// Ctrl+Space pressed: open the popup manually (empty prefix = list).
    CompletionTrigger,
    /// Esc pressed while the completion popup is open.
    CompletionClose,
    /// Switch between the light and dark chrome (persisted).
    ToggleTheme,
    /// Open the connection form pre-filled with the selected entry.
    EditSelected,
    /// The background test finished; Ok carries a short success blurb.
    Tested(Result<String, String>),
    /// Right-click on a navigator row: open its context menu.
    ContextOpen(MenuTarget),
    /// Close the context menu (Esc, ✕ or a plain click in the tree).
    ContextClose,
    /// Context menu choice; `ConfirmDelete` only runs after the menu's
    /// second, destructive step.
    ContextAct(ContextAct),
    /// `DROP TABLE/VIEW` finished for `ObjectRef` (Ok) or failed (Err).
    ObjectDropped(ObjectRef, Result<(), String>),
}

/// Right-click menu shown right under a navigator row.
struct ContextMenu {
    target: MenuTarget,
    /// Second step of "Eliminar…": destructive confirmation is showing.
    confirm: bool,
}

/// What a context menu points at.
#[derive(Clone, Debug)]
enum MenuTarget {
    Conn(ConnectionId),
    Node(ObjectRef, ObjectKind),
}

/// Entries of the navigator context menu.
#[derive(Clone, Copy, Debug)]
enum ContextAct {
    /// Put a starter query in the editor (table/view/collection).
    Open,
    /// Open the table designer for a table.
    Design,
    /// New table under a database/schema (or from a connection row).
    NewTable,
    /// Force a fresh children listing (connection root or node).
    Refresh,
    /// Copy the row's name to the clipboard.
    CopyName,
    /// Open the connection form for this connection.
    EditConn,
    /// Show the destructive confirmation step.
    AskDelete,
    /// Run DROP (object) or remove the connection.
    ConfirmDelete,
    /// Leave the confirmation step (closes the menu).
    Cancel,
}

type ConnectedPayload = (ConnectionId, Arc<Session>, ObjectRef, Vec<ObjectNode>);

/// Ok payload of `DesignerLoaded`: connection, the table being described
/// and its structure. Err is a plain string so batches stay attributable
/// without carrying the request context.
type DesignerPayload = (ConnectionId, ObjectRef, drivers::TableDetail);

/// Ok payload of `SyncLoaded`: source table + structure, destination
/// connection/table and its structure (`None` = table missing there, so
/// the plan is a plain CREATE TABLE).
type SyncPayload = (
    ObjectRef,
    drivers::TableDetail,
    ConnectionId,
    ObjectRef,
    Option<drivers::TableDetail>,
);

/// Open structure-sync overlay: both sides loaded and ready to diff.
#[derive(Clone, Debug)]
struct SyncState {
    source: ObjectRef,
    source_detail: drivers::TableDetail,
    target_conn: ConnectionId,
    target: ObjectRef,
    target_detail: Option<drivers::TableDetail>,
}

/// SQL completion popup: the word being typed, the dictionary it filters and
/// the highlighted candidate. The dictionary is loaded once per connection by
/// `load_completion_words`.
#[derive(Default)]
struct Completion {
    /// Table/column/view words for the active connection.
    dictionary: Vec<String>,
    /// Characters typed after the last word boundary.
    prefix: String,
    /// Candidates matching `prefix` (case-insensitive, capped).
    items: Vec<String>,
    /// Index highlighted in the popup.
    selected: usize,
    open: bool,
}

impl Completion {
    /// Characters that extend the current word ("SELECT usu|").
    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '.'
    }

    fn push_char(&mut self, c: char) {
        self.prefix.push(c);
        self.recompute();
    }

    fn pop_char(&mut self) {
        self.prefix.pop();
        self.recompute();
    }

    /// Discard the tracked prefix (cursor moved, mouse click, etc.).
    fn invalidate(&mut self) {
        self.prefix.clear();
        self.close();
    }

    fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
    }

    /// Open with the whole dictionary (Ctrl+Space with no prefix).
    fn open_all(&mut self) {
        if self.dictionary.is_empty() {
            self.close();
            return;
        }
        self.items = self.dictionary.iter().take(50).cloned().collect();
        self.selected = 0;
        self.open = true;
    }

    fn recompute(&mut self) {
        if self.prefix.is_empty() {
            self.close();
            return;
        }
        let needle = self.prefix.to_lowercase();
        self.items = self
            .dictionary
            .iter()
            .filter(|w| w.to_lowercase().starts_with(&needle))
            .take(50)
            .cloned()
            .collect();
        self.selected = 0;
        self.open = !self.items.is_empty();
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1).min(self.items.len() - 1);
        }
    }

    fn selected_word(&self) -> Option<&str> {
        self.items.get(self.selected).map(String::as_str)
    }
}

fn boot() -> (App, Task<Message>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let settings = store::load_settings();
    let dark = settings.theme_dark;

    // Derive the editor palette from the app theme so the code pane matches
    // the chrome, then lift the surfaces to pure white (light) / slate (dark).
    let mut editor = CodeEditor::new(SAMPLE_SQL, "sql");
    let mut editor_style = iced_code_editor::from_iced_theme(&theme::app_theme(dark));
    let palette = theme::editor_palette(dark);
    editor_style.background = palette.background;
    editor_style.gutter_background = palette.gutter_background;
    editor.set_theme(editor_style);

    // Saved connections: configs from disk, passwords from the keyring.
    let store = Connections::open();
    let entries: Vec<ConnEntry> = store
        .load()
        .into_iter()
        .map(|cfg| ConnEntry {
            cfg,
            state: ConnState::Idle,
        })
        .collect();
    let names: Vec<String> = entries.iter().map(|e| e.cfg.name.clone()).collect();
    let status = if entries.is_empty() {
        "listo — «+ Nueva conexión» para empezar".to_string()
    } else {
        format!("{} conexión(es) guardada(s)", entries.len())
    };

    (
        App {
            rt: Arc::new(rt),
            store,
            editor,
            grid: grid::empty(),
            search: AutocompleteState::new(names, Filter::None),
            search_text: String::new(),
            entries,
            selected: None,
            form: None,
            sessions: HashMap::new(),
            active_id: None,
            tabs: vec![Tab::new("Consulta 1".to_string(), None)],
            active_tab: 0,
            next_tab: 2,
            er: None,
            designer: None,
            design_pending: None,
            sync: None,
            sync_pending: None,
            tree_filter: String::new(),
            expanded_root: true,
            context_menu: None,
            selected_object: None,
            root: None,
            tree: HashMap::new(),
            expanded: HashSet::new(),
            last_columns: Vec::new(),
            last_row_count: 0,
            executed: false,
            query_context: None,
            completion: Completion::default(),
            completion_words: HashMap::new(),
            dark,
            editing: None,
            status,
            window_id: None,
            last_title_press: None,
        },
        Task::none(),
    )
}

fn connect_task(rt: Arc<tokio::runtime::Runtime>, cfg: ConnectionConfig) -> Task<Message> {
    let id = cfg.id;
    let root = ObjectRef::root(id);
    let root_reply = root.clone();
    Task::perform(
        async move {
            match rt
                .spawn(async move {
                    let session = drivers::connect(&cfg).await.map_err(|e| e.to_string())?;
                    let children = session
                        .client
                        .list_children(&root)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>((Arc::new(session), children))
                })
                .await
            {
                Ok(Ok((session, children))) => Ok((id, session, root_reply, children)),
                Ok(Err(e)) => Err((id, e)),
                Err(join) => Err((id, join.to_string())),
            }
        },
        Message::Connected,
    )
}

/// "Probar" in the form: handshake + ping + drop, nothing saved.
fn test_task(rt: Arc<tokio::runtime::Runtime>, cfg: ConnectionConfig) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                let session = drivers::connect(&cfg).await.map_err(|e| e.to_string())?;
                session
                    .client
                    .ping()
                    .await
                    .map_err(|e| e.to_string())?;
                let host = if cfg.ssh.is_some() {
                    format!("{} (vía SSH)", cfg.host)
                } else {
                    cfg.host.clone()
                };
                Ok::<_, String>(format!("test OK: {} → {host}:{}", cfg.name, cfg.port))
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::Tested,
    )
}

fn execute_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: std::sync::Arc<dyn drivers::DatabaseClient>,
    req: ExecuteRequest,
    tab: usize,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move { client.execute(req).await.map_err(|e| e.to_string()) })
                .await
                .map_err(|e| e.to_string())?
        },
        move |res| Message::Executed(tab, res),
    )
}

/// Which tab index becomes active after closing `closed` in a strip of
/// `len` tabs where `active` was focused.
fn active_after_close(len: usize, active: usize, closed: usize) -> usize {
    debug_assert!(len > 1, "single-tab strips are recreated instead");
    if closed < active {
        active - 1
    } else if closed == active {
        // Focus falls to the neighbour; at the tail, step back.
        if active == len - 1 {
            active - 1
        } else {
            active
        }
    } else {
        active
    }
}

/// True when `k` is `target` itself or lives under it (same connection).
/// Used to drop stale navigator caches after a DDL apply.
fn is_under(target: &ObjectRef, k: &ObjectRef) -> bool {
    k.connection == target.connection
        && k.path.len() >= target.path.len()
        && k.path[..target.path.len()] == target.path[..]
}

/// Navigator filter: does this label pass? Empty filters pass everything.
/// Matching is a case-insensitive substring test (as in Navicat's tree
/// search box).
fn node_matches(label: &str, filter: &str) -> bool {
    let f = filter.trim();
    f.is_empty() || label.to_lowercase().contains(&f.to_lowercase())
}

/// True when any *loaded* descendant of `node` matches the filter (so the
/// tree can keep the branch visible without forcing lazy loads).
fn subtree_matches(app: &App, node: &ObjectRef, filter: &str) -> bool {
    let Some(children) = app.tree.get(node) else {
        return false;
    };
    children.iter().any(|c| {
        node_matches(&c.label, filter) || subtree_matches(app, &c.object, filter)
    })
}

/// Container a new table would live in, derived from the last object the
/// user opened: its database/schema, its parent (for tables), or the
/// connection root when nothing is selected.
fn design_parent(id: ConnectionId, selected: Option<&(ObjectRef, ObjectKind)>) -> ObjectRef {
    match selected {
        Some((obj, kind))
            if obj.connection == id
                && matches!(kind, ObjectKind::Database | ObjectKind::Schema) =>
        {
            obj.clone()
        }
        Some((obj, kind))
            if obj.connection == id && matches!(kind, ObjectKind::Table | ObjectKind::View) =>
        {
            obj.parent().unwrap_or_else(|| ObjectRef::root(id))
        }
        _ => ObjectRef::root(id),
    }
}

/// Discover the connection's tables and describe them all in one hop.
fn er_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    root: ObjectRef,
    connection: ConnectionId,
) -> Task<Message> {
    Task::perform(
        async move {
            let res: Result<_, String> = rt
                .spawn(async move {
                    er::load(&*client, &root).await.map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())?;
            res.map(|(tables, failed)| er::ErPayload {
                connection,
                tables,
                failed,
            })
        },
        Message::ErLoaded,
    )
}

/// Describe one table for the designer overlay.
fn design_describe_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    table: ObjectRef,
) -> Task<Message> {
    let reply = table.clone();
    Task::perform(
        async move {
            let res: Result<_, String> = rt
                .spawn(async move {
                    client
                        .describe_table(&table)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())?;
            res.map(|detail| (reply, detail))
        },
        move |res| {
            Message::DesignerLoaded(res.map(|(table, detail)| {
                (table.connection, table, detail)
            }))
        },
    )
}

/// Run the designer's DDL statements sequentially; `Ok(n)` counts the ones
/// that completed before any failure stopped the batch.
fn design_apply_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    stmts: Vec<String>,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                let mut applied = 0usize;
                for stmt in stmts {
                    client
                        .execute(ExecuteRequest::new(&stmt))
                        .await
                        .map_err(|e| format!("{stmt}\n  → {e}"))?;
                    applied += 1;
                }
                Ok::<usize, String>(applied)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::DesignApplied,
    )
}

/// Run a single `DROP TABLE/VIEW` for the right-click "Eliminar…" action;
/// `ObjectDropped` carries the targeted object so the tree can refresh.
fn drop_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    stmt: String,
    obj: ObjectRef,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                client
                    .execute(ExecuteRequest::new(&stmt))
                    .await
                    .map_err(|e| format!("{stmt}\n  → {e}"))?;
                Ok::<(), String>(())
            })
            .await
            .map_err(|e| e.to_string())?
        },
        move |res| Message::ObjectDropped(obj.clone(), res),
    )
}

/// Describe both sides of a structure sync. A destination table that does
/// not exist (or cannot be read) is not an error: the plan falls back to
/// `CREATE TABLE`.
fn sync_task(
    rt: Arc<tokio::runtime::Runtime>,
    src_client: Arc<dyn drivers::DatabaseClient>,
    source: ObjectRef,
    dst_client: Arc<dyn drivers::DatabaseClient>,
    target: ObjectRef,
    target_conn: ConnectionId,
) -> Task<Message> {
    Task::perform(
        async move {
            let rt2 = rt.clone();
            let (source, source_detail) = rt
                .spawn({
                    let source = source.clone();
                    async move {
                        src_client
                            .describe_table(&source)
                            .await
                            .map(|d| (source, d))
                            .map_err(|e| e.to_string())
                    }
                })
                .await
                .map_err(|e| e.to_string())??;
            let target_ref = target.clone();
            let target_detail = rt2
                .spawn(async move {
                    dst_client
                        .describe_table(&target_ref)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
                .ok()
                .and_then(Result::ok);
            Ok::<SyncPayload, String>((
                source,
                source_detail,
                target_conn,
                target,
                target_detail,
            ))
        },
        Message::SyncLoaded,
    )
}

/// Run migration statements on a destination connection for the sync
/// overlay (same sequential semantics as `design_apply_task`).
fn sync_apply_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    stmts: Vec<String>,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                let mut applied = 0usize;
                for stmt in stmts {
                    client
                        .execute(ExecuteRequest::new(&stmt))
                        .await
                        .map_err(|e| format!("{stmt}\n  → {e}"))?;
                    applied += 1;
                }
                Ok::<usize, String>(applied)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::SyncApplied,
    )
}

impl App {
    fn refresh_search(&mut self) {
        let names: Vec<String> = self.entries.iter().map(|e| e.cfg.name.clone()).collect();
        let needle = self.search_text.to_ascii_lowercase();
        let options: Vec<String> = names
            .iter()
            .filter(|n| n.to_ascii_lowercase().contains(&needle))
            .cloned()
            .collect();
        self.search.set_options(options);
    }

    /// Save every connection config (passwords to the keyring, not disk).
    fn persist(&mut self) {
        let cfgs: Vec<ConnectionConfig> = self.entries.iter().map(|e| e.cfg.clone()).collect();
        if let Err(e) = self.store.save(&cfgs) {
            self.status = format!("aviso: no se pudo guardar: {e}");
        }
    }

    /// Replace the tracked word with the highlighted candidate: erase the
    /// typed prefix with synthetic Backspaces, then insert the word char by
    /// char. Both go straight to the editor (not through `update`), so the
    /// completion interception never sees them.
    fn accept_completion(&mut self) -> Task<Message> {
        let Some(word) = self.completion.selected_word().map(str::to_owned) else {
            return Task::none();
        };
        for _ in 0..self.completion.prefix.chars().count() {
            let _ = self.editor.update(&EditorMessage::Backspace);
        }
        for ch in word.chars() {
            let _ = self.editor.update(&EditorMessage::CharacterInput(ch));
        }
        // The old prefix is gone (we just rewrote it); track from scratch.
        self.completion.invalidate();
        Task::none()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Editor(msg) => {
                use iced_code_editor::ArrowDirection as Dir;
                match &msg {
                    // Typing extends the word and refilters the candidates.
                    EditorMessage::CharacterInput(c) if Completion::is_word_char(*c) => {
                        self.completion.push_char(*c);
                        let _ = self.editor.update(&msg);
                        Task::none()
                    }
                    // A non-word char (space, paren, …) ends the word.
                    EditorMessage::CharacterInput(_) => {
                        self.completion.invalidate();
                        let _ = self.editor.update(&msg);
                        Task::none()
                    }
                    // Tab/Enter accept the highlighted candidate instead of
                    // indenting / inserting a newline.
                    EditorMessage::Tab if self.completion.open => self.accept_completion(),
                    EditorMessage::Enter if self.completion.open => self.accept_completion(),
                    // Up/Down navigate the popup without moving the cursor.
                    EditorMessage::ArrowKey(Dir::Up, false) if self.completion.open => {
                        self.completion.move_up();
                        Task::none()
                    }
                    EditorMessage::ArrowKey(Dir::Down, false) if self.completion.open => {
                        self.completion.move_down();
                        Task::none()
                    }
                    // Any other cursor movement invalidates the tracked word.
                    EditorMessage::Backspace => {
                        self.completion.pop_char();
                        let _ = self.editor.update(&msg);
                        Task::none()
                    }
                    EditorMessage::Delete
                    | EditorMessage::MouseClick(_)
                    | EditorMessage::ArrowKey(..)
                    | EditorMessage::Home(_)
                    | EditorMessage::End(_)
                    | EditorMessage::PageUp(_)
                    | EditorMessage::PageDown(_) => {
                        self.completion.invalidate();
                        let _ = self.editor.update(&msg);
                        Task::none()
                    }
                    _ => {
                        let _ = self.editor.update(&msg);
                        Task::none()
                    }
                }
            }
            Message::Grid(msg) => self.grid.update(msg).map(Message::Grid),
            Message::SearchInput(text) => {
                self.search_text = text;
                self.refresh_search();
                Task::none()
            }
            Message::SearchSubmit(text) => {
                self.status = format!("buscar: {text}");
                Task::none()
            }
            Message::SearchSelect(name) => {
                self.search_text = name.clone();
                if let Some(i) = self.entries.iter().position(|e| e.cfg.name == name) {
                    return self.update(Message::SelectConnection(i));
                }
                Task::none()
            }
            Message::SelectConnection(i) => self.select_connection(i),
            Message::TreeToggleConn(id) => {
                self.context_menu = None;
                let Some(i) = self.entries.iter().position(|e| e.cfg.id == id) else {
                    return Task::none();
                };
                let root = ObjectRef::root(id);
                if self.active_id == Some(id) && self.expanded.contains(&root) {
                    self.expanded.remove(&root);
                    self.selected = Some(i);
                    return Task::none();
                }
                self.selected = Some(i);
                self.expanded.insert(root);
                self.select_connection(i)
            }
            Message::TreeRootToggle => {
                self.context_menu = None;
                self.expanded_root = !self.expanded_root;
                Task::none()
            }
            Message::TreeFilter(f) => {
                self.tree_filter = f;
                Task::none()
            }
            Message::ContextOpen(target) => {
                self.context_menu = Some(ContextMenu {
                    target,
                    confirm: false,
                });
                Task::none()
            }
            Message::ContextClose => {
                self.context_menu = None;
                Task::none()
            }
            Message::ContextAct(act) => {
                // Taking the menu closes it; only AskDelete re-opens it in
                // its destructive second step.
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                match act {
                    ContextAct::AskDelete => {
                        self.context_menu = Some(ContextMenu {
                            target: menu.target,
                            confirm: true,
                        });
                        Task::none()
                    }
                    ContextAct::Cancel => Task::none(),
                    ContextAct::CopyName => {
                        let name = match &menu.target {
                            MenuTarget::Conn(id) => self
                                .entries
                                .iter()
                                .find(|e| e.cfg.id == *id)
                                .map(|e| e.cfg.name.clone()),
                            MenuTarget::Node(obj, _) => obj.path.last().cloned(),
                        };
                        match name {
                            Some(n) => {
                                self.status = format!("copiado: {n}");
                                iced::clipboard::write(n)
                            }
                            None => Task::none(),
                        }
                    }
                    ContextAct::EditConn => {
                        let MenuTarget::Conn(id) = menu.target else {
                            return Task::none();
                        };
                        let Some(i) =
                            self.entries.iter().position(|e| e.cfg.id == id)
                        else {
                            return Task::none();
                        };
                        self.selected = Some(i);
                        self.update(Message::EditSelected)
                    }
                    ContextAct::Refresh => match &menu.target {
                        MenuTarget::Conn(id) => {
                            let id = *id;
                            let Some(i) =
                                self.entries.iter().position(|e| e.cfg.id == id)
                            else {
                                return Task::none();
                            };
                            if self.sessions.contains_key(&id) {
                                let root = ObjectRef::root(id);
                                self.tree.remove(&root);
                                self.load_children(&root)
                            } else {
                                self.select_connection(i)
                            }
                        }
                        MenuTarget::Node(obj, _) => {
                            let obj = obj.clone();
                            self.reload_children(&obj)
                        }
                    },
                    ContextAct::Open => {
                        let MenuTarget::Node(obj, kind) = menu.target else {
                            return Task::none();
                        };
                        if !matches!(
                            kind,
                            ObjectKind::Table
                                | ObjectKind::View
                                | ObjectKind::Collection
                        ) {
                            return Task::none();
                        }
                        self.update(Message::TreeOpenObject(obj, kind))
                    }
                    ContextAct::Design => {
                        let MenuTarget::Node(obj, kind) = menu.target else {
                            return Task::none();
                        };
                        if kind != ObjectKind::Table {
                            return Task::none();
                        }
                        self.selected_object = Some((obj, kind));
                        self.update(Message::DesignSelected)
                    }
                    ContextAct::NewTable => match &menu.target {
                        MenuTarget::Node(obj, kind)
                            if matches!(
                                kind,
                                ObjectKind::Database | ObjectKind::Schema
                            ) =>
                        {
                            let (obj, kind) = (obj.clone(), *kind);
                            self.selected_object = Some((obj, kind));
                            self.update(Message::DesignNew)
                        }
                        MenuTarget::Conn(id) => {
                            let id = *id;
                            let Some(i) =
                                self.entries.iter().position(|e| e.cfg.id == id)
                            else {
                                return Task::none();
                            };
                            let bind = self.select_connection(i);
                            let open = self.update(Message::DesignNew);
                            Task::batch([bind, open])
                        }
                        _ => Task::none(),
                    },
                    ContextAct::ConfirmDelete => match menu.target {
                        MenuTarget::Conn(id) => {
                            let Some(i) =
                                self.entries.iter().position(|e| e.cfg.id == id)
                            else {
                                return Task::none();
                            };
                            self.selected = Some(i);
                            self.update(Message::DeleteSelected)
                        }
                        MenuTarget::Node(obj, kind)
                            if matches!(kind, ObjectKind::Table | ObjectKind::View) =>
                        {
                            let engine = self.engine_of(obj.connection);
                            if engine == Engine::Mongodb {
                                return Task::none();
                            }
                            let Some(stmt) = drop_statement(engine, kind, &obj)
                            else {
                                return Task::none();
                            };
                            let Some(session) =
                                self.sessions.get(&obj.connection).cloned()
                            else {
                                self.status =
                                    "sin sesión: no se puede eliminar".into();
                                return Task::none();
                            };
                            let name = obj
                                .path
                                .last()
                                .cloned()
                                .unwrap_or_default();
                            self.status = format!("eliminando {name}…");
                            drop_task(
                                self.rt.clone(),
                                session.client.clone(),
                                stmt,
                                obj,
                            )
                        }
                        _ => Task::none(),
                    },
                }
            }
            Message::ObjectDropped(obj, res) => match res {
                Ok(()) => {
                    let name = obj.path.last().cloned().unwrap_or_default();
                    self.status = format!("✓ eliminada: {name}");
                    self.invalidate_after_ddl(&obj);
                    // Re-list the parent right away so the row vanishes
                    // without forcing the user to collapse/expand it.
                    if let Some(parent) = obj.parent() {
                        self.tree.remove(&parent);
                        self.expanded.remove(&parent);
                        let reload = self.expand(&parent);
                        return reload;
                    }
                    Task::none()
                }
                Err(e) => {
                    self.status = format!("error al eliminar: {e}");
                    Task::none()
                }
            },
            Message::DeleteSelected => {
                let Some(i) = self.selected.take() else {
                    return Task::none();
                };
                if i >= self.entries.len() {
                    return Task::none();
                }
                let removed = self.entries.remove(i);
                self.sessions.remove(&removed.cfg.id);
                self.completion_words.remove(&removed.cfg.id);
                if self.er.as_ref().is_some_and(|e| e.connection == removed.cfg.id) {
                    self.er = None;
                }
                if self.designer.as_ref().is_some_and(|d| d.connection == removed.cfg.id) {
                    self.designer = None;
                }
                if self.design_pending == Some(removed.cfg.id) {
                    self.design_pending = None;
                }
                if self.sync.as_ref().is_some_and(|s| {
                    s.source.connection == removed.cfg.id || s.target_conn == removed.cfg.id
                }) {
                    self.sync = None;
                }
                if self.sync_pending == Some(removed.cfg.id) {
                    self.sync_pending = None;
                }
                if self
                    .selected_object
                    .as_ref()
                    .is_some_and(|(o, _)| o.connection == removed.cfg.id)
                {
                    self.selected_object = None;
                }
                for tab in &mut self.tabs {
                    if tab.connection == Some(removed.cfg.id) {
                        tab.connection = None;
                    }
                }
                if self.active_id == Some(removed.cfg.id) {
                    // Dropping the session closes its tunnel/client.
                    self.active_id = None;
                    self.root = None;
                    self.tree.retain(|k, _| k.connection != removed.cfg.id);
                    self.expanded.retain(|k| k.connection != removed.cfg.id);
                    self.query_context = None;
                    self.completion.dictionary.clear();
                    self.completion.close();
                }
                // No auto-reselect: a second Delete must be preceded by an
                // explicit click, so one stray keypress can't cascade.
                self.selected = None;
                self.search_text.clear();
                self.refresh_search();
                self.store.forget(&removed.cfg);
                self.status = format!("conexión «{}» eliminada", removed.cfg.name);
                self.persist();
                Task::none()
            }
            Message::Run => {
                let Some(id) = self.active_id else {
                    self.status =
                        "sin conexión — selecciona una en la barra lateral".to_string();
                    return Task::none();
                };
                let Some(session) = self.sessions.get(&id) else {
                    self.status = "la conexión no está activa — selecciónala de nuevo".to_string();
                    return Task::none();
                };
                let sql = self.editor.content();
                if sql.trim().is_empty() {
                    self.status = "el editor está vacío".to_string();
                    return Task::none();
                }
                let client = session.client.clone();
                self.status = "ejecutando…".to_string();
                let mut req = ExecuteRequest::new(sql).with_max_rows(MAX_ROWS);
                req.context = self.query_context.clone();
                execute_task(self.rt.clone(), client, req, self.active_tab)
            }
            Message::OpenForm => {
                self.form = Some(ConnForm::default());
                self.editing = None;
                Task::none()
            }
            Message::EditSelected => {
                let Some(i) = self.selected else {
                    return Task::none();
                };
                let Some(entry) = self.entries.get(i) else {
                    return Task::none();
                };
                self.form = Some(ConnForm::from_cfg(&entry.cfg));
                self.editing = Some(i);
                Task::none()
            }
            Message::ToggleTheme => {
                self.dark = !self.dark;
                let palette = theme::editor_palette(self.dark);
                let mut editor_style =
                    iced_code_editor::from_iced_theme(&theme::app_theme(self.dark));
                editor_style.background = palette.background;
                editor_style.gutter_background = palette.gutter_background;
                self.editor.set_theme(editor_style);
                store::save_settings(&store::Settings {
                    theme_dark: self.dark,
                });
                Task::none()
            }
            Message::Form(FormMsg::Test) => {
                // "Probar": build a config from the live fields and dial it in
                // the background without touching the saved list.
                let Some(form) = &self.form else {
                    return Task::none();
                };
                match form.build() {
                    Ok(cfg) => {
                        self.status = format!("probando {}…", cfg.name);
                        test_task(self.rt.clone(), cfg)
                    }
                    Err(e) => {
                        self.status = format!("error: {e}");
                        Task::none()
                    }
                }
            }
            Message::Tested(Ok(blurb)) => {
                self.status = format!("✓ {blurb}");
                Task::none()
            }
            Message::Tested(Err(e)) => {
                self.status = format!("✗ test falló: {e}");
                Task::none()
            }
            Message::Form(FormMsg::Cancel) => {
                self.form = None;
                self.editing = None;
                Task::none()
            }
            Message::Form(FormMsg::Submit) => {
                let Some(form) = self.form.take() else {
                    return Task::none();
                };
                match form.build() {
                    Ok(mut cfg) => {
                        if let Some(i) = self.editing.take() {
                            // Keep identity and in-place the entry; the old
                            // session/caches for it are stale and get dropped.
                            let old = &self.entries[i].cfg;
                            cfg.id = old.id;
                            self.sessions.remove(&cfg.id);
                            self.completion_words.remove(&cfg.id);
                            self.tree.retain(|k, _| k.connection != cfg.id);
                            self.expanded.retain(|k| k.connection != cfg.id);
                            if self.designer.as_ref().is_some_and(|d| d.connection == cfg.id) {
                                self.designer = None;
                            }
                            if self.design_pending == Some(cfg.id) {
                                self.design_pending = None;
                            }
                            if self.sync.as_ref().is_some_and(|s| {
                                s.source.connection == cfg.id || s.target_conn == cfg.id
                            }) {
                                self.sync = None;
                            }
                            if self.sync_pending == Some(cfg.id) {
                                self.sync_pending = None;
                            }
                            if self.active_id == Some(cfg.id) {
                                self.root = None;
                                self.query_context = None;
                                self.completion.dictionary.clear();
                                self.completion.close();
                            }
                            self.entries[i].cfg = cfg.clone();
                            self.entries[i].state = ConnState::Connecting;
                            self.selected = Some(i);
                            self.persist();
                            self.status = format!("guardada — conectando a {}…", cfg.name);
                            connect_task(self.rt.clone(), cfg)
                        } else {
                            // New connection: bind it to the active tab so
                            // Connected adopts it as the live workspace.
                            self.active_id = Some(cfg.id);
                            self.root = Some(ObjectRef::root(cfg.id));
                            self.entries.push(ConnEntry {
                                cfg: cfg.clone(),
                                state: ConnState::Connecting,
                            });
                            self.selected = Some(self.entries.len() - 1);
                            self.search_text = cfg.name.clone();
                            self.refresh_search();
                            self.persist();
                            self.status = format!("conectando a {}…", cfg.name);
                            connect_task(self.rt.clone(), cfg)
                        }
                    }
                    Err(e) => {
                        self.form = Some(form);
                        self.status = format!("error: {e}");
                        Task::none()
                    }
                }
            }
            Message::Form(msg) => {
                if let Some(form) = &mut self.form {
                    form.update(msg);
                }
                Task::none()
            }
            Message::Connected(Ok((id, session, root, children))) => {
                if !self.entries.iter().any(|e| e.cfg.id == id) {
                    // Deleted while the handshake was in flight.
                    self.status = "conexión descartada (fue eliminada)".to_string();
                    return Task::none();
                }
                if let Some(e) = self.entries.iter_mut().find(|e| e.cfg.id == id) {
                    e.state = ConnState::Connected;
                }
                // Per-connection refresh: stale children/expansion from a
                // previous session are dropped, the fresh root is kept.
                self.tree.retain(|k, _| k.connection != id);
                self.expanded.retain(|k| k.connection != id);
                self.tree.insert(root.clone(), children);
                self.expanded.insert(root.clone());
                let load_session = session.clone();
                self.sessions.insert(id, session);
                self.status = match self.entries.iter().find(|e| e.cfg.id == id) {
                    Some(e) => format!("conectada: {} ({})", e.cfg.name, e.cfg.engine),
                    None => "conectada".to_string(),
                };
                // Adopt into the live workspace only when this connection is
                // what the active tab binds; otherwise the caches above wait
                // for the tab to switch back (load_tab adopts from them).
                if self.active_id == Some(id) {
                    self.root = Some(root.clone());
                    self.query_context = None;
                    self.completion.close();
                    self.completion.dictionary = self
                        .completion_words
                        .get(&id)
                        .cloned()
                        .unwrap_or_default();
                }
                Task::perform(
                    load_completion_words(load_session, root),
                    move |words| Message::CompletionWordsLoaded(id, words),
                )
            }
            Message::Connected(Err((id, e))) => {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.cfg.id == id) {
                    entry.state = ConnState::Error(e.clone());
                }
                self.status = format!("error de conexión: {e}");
                Task::none()
            }
            Message::TreeToggle(node, kind) => {
                // A plain click dismisses any context menu.
                self.context_menu = None;
                // Expanding a database/schema also selects it: it seeds the
                // parent for "Nueva tabla" and shows where you are.
                if matches!(kind, ObjectKind::Database | ObjectKind::Schema) {
                    self.selected_object = Some((node.clone(), kind));
                }
                self.expand(&node)
            }
            Message::TreeSelect(node, kind) => {
                self.context_menu = None;
                self.selected_object = Some((node, kind));
                Task::none()
            }
            Message::TreeOpenObject(obj, kind) => {
                self.context_menu = None;
                let sql = self.query_for(&obj, kind);
                self.query_context = match kind {
                    ObjectKind::Collection => Some(obj.path.join(".")),
                    _ => None,
                };
                self.selected_object = Some((obj.clone(), kind));
                let expand = self.expand(&obj);
                self.completion.close();
                let edit = self.editor.reset(&sql).map(Message::Editor);
                Task::batch([expand, edit])
            }
            Message::ChildrenLoaded(parent, Ok(children)) => {
                self.tree.insert(parent, children);
                Task::none()
            }
            Message::ChildrenLoaded(_parent, Err(e)) => {
                self.status = format!("error al listar objetos: {e}");
                Task::none()
            }
            Message::CompletionWordsLoaded(id, words) => {
                let n = words.len();
                if self.active_id == Some(id) {
                    self.completion.dictionary = words.clone();
                    self.status = format!("conexión lista (autocompletado: {n} objetos)");
                }
                self.completion_words.insert(id, words);
                Task::none()
            }
            Message::CompletionAccept(index) => {
                if index < self.completion.items.len() {
                    self.completion.selected = index;
                    self.accept_completion()
                } else {
                    Task::none()
                }
            }
            Message::CompletionTrigger => {
                self.completion.open_all();
                Task::none()
            }
            Message::CompletionClose => {
                if self.completion.open {
                    self.completion.close();
                } else if self.context_menu.is_some() {
                    // Esc with no popup up dismisses the context menu.
                    self.context_menu = None;
                }
                Task::none()
            }
            Message::Executed(tab, res) => match res {
                Ok(result) => self.apply_result(tab, result),
                Err(e) => {
                    // Errors only surface on the tab that ran them.
                    if tab == self.active_tab {
                        self.status = format!("error: {e}");
                    }
                    Task::none()
                }
            },
            Message::NewTab => {
                self.stash_tab();
                let title = format!("Consulta {}", self.next_tab);
                self.next_tab += 1;
                let connection = self.active_id;
                self.tabs.push(Tab::new(title, connection));
                self.active_tab = self.tabs.len() - 1;
                self.load_tab(self.active_tab)
            }
            Message::CloseTab(index) => {
                if index >= self.tabs.len() {
                    return Task::none();
                }
                self.stash_tab();
                if self.tabs.len() == 1 {
                    // Last tab closed → start a fresh, unbound one.
                    self.tabs.remove(0);
                    self.tabs
                        .push(Tab::new(format!("Consulta {}", self.next_tab), None));
                    self.next_tab += 1;
                    self.active_tab = 0;
                } else {
                    // Compute the successor before mutating the strip.
                    let new_active =
                        active_after_close(self.tabs.len(), self.active_tab, index);
                    self.tabs.remove(index);
                    self.active_tab = new_active;
                }
                self.load_tab(self.active_tab)
            }
            Message::SelectTab(index) => {
                if index >= self.tabs.len() || index == self.active_tab {
                    return Task::none();
                }
                self.stash_tab();
                self.active_tab = index;
                self.load_tab(index)
            }
            Message::ErOpen => {
                let Some(id) = self.active_id else {
                    self.status = "selecciona una conexión para el diagrama ER".to_string();
                    return Task::none();
                };
                let Some(session) = self.sessions.get(&id).cloned() else {
                    self.status = "la conexión no está activa — selecciónala de nuevo".to_string();
                    return Task::none();
                };
                self.completion.close();
                self.designer = None;
                self.design_pending = None;
                self.sync = None;
                self.sync_pending = None;
                self.er = Some(ErState {
                    connection: id,
                    diagram: None,
                    loading: true,
                    failed: Vec::new(),
                });
                self.status = "cargando diagrama ER…".to_string();
                er_task(self.rt.clone(), session.client.clone(), ObjectRef::root(id), id)
            }
            Message::ErLoaded(Ok(payload)) => {
                // Build the status first so the `ErState` borrow can end.
                let status = match self.er.as_mut() {
                    Some(er) if er.connection == payload.connection => {
                        er.loading = false;
                        er.failed = payload.failed;
                        let diagram = if payload.tables.is_empty() {
                            None
                        } else {
                            Some(er::Diagram::new(payload.tables))
                        };
                        let (tables, rels) =
                            diagram.as_ref().map_or((0, 0), |d| (d.tables.len(), d.edges.len()));
                        er.diagram = diagram;
                        if tables == 0 {
                            if er.failed.is_empty() {
                                "diagrama ER: sin tablas en esta conexión".to_string()
                            } else {
                                format!("diagrama ER: {}", er.failed.join(" · "))
                            }
                        } else {
                            let note = if er.failed.is_empty() {
                                String::new()
                            } else {
                                format!(" ({} avisos)", er.failed.len())
                            };
                            format!("diagrama ER: {tables} tablas, {rels} relaciones{note}")
                        }
                    }
                    // Stale batch (diagrama cerrado o de otra conexión).
                    _ => String::new(),
                };
                if !status.is_empty() {
                    self.status = status;
                }
                Task::none()
            }
            Message::ErLoaded(Err(e)) => {
                if let Some(er) = self.er.as_mut() {
                    er.loading = false;
                }
                self.status = format!("error al cargar el diagrama: {e}");
                Task::none()
            }
            Message::ErClose => {
                self.er = None;
                Task::none()
            }
            Message::DesignNew => {
                let Some(id) = self.active_id else {
                    self.status = "selecciona una conexión para diseñar una tabla".to_string();
                    return Task::none();
                };
                let engine = self.engine_of(id);
                if engine == Engine::Mongodb {
                    self.status =
                        "MongoDB no usa diseñador de tablas (esquema dinámico)".to_string();
                    return Task::none();
                }
                let parent = design_parent(id, self.selected_object.as_ref());
                self.er = None;
                self.sync = None;
                self.sync_pending = None;
                self.completion.close();
                self.design_pending = None;
                self.designer = Some(designer::DesignerState::new_table(id, parent));
                self.status = "diseñador: nueva tabla — define columnas y aplica".to_string();
                Task::none()
            }
            Message::DesignSelected => {
                let Some((obj, kind)) = self.selected_object.clone() else {
                    self.status = "abre una tabla en el árbol para diseñarla".to_string();
                    return Task::none();
                };
                if kind != ObjectKind::Table {
                    self.status = "selecciona una tabla (no una vista)".to_string();
                    return Task::none();
                }
                let Some(session) = self.sessions.get(&obj.connection).cloned() else {
                    self.status = "la conexión de esa tabla no está activa".to_string();
                    return Task::none();
                };
                self.er = None;
                self.sync = None;
                self.sync_pending = None;
                self.completion.close();
                self.design_pending = Some(obj.connection);
                self.status = format!("cargando estructura de {}…", obj.path.last().cloned().unwrap_or_default());
                design_describe_task(self.rt.clone(), session.client.clone(), obj)
            }
            Message::DesignerLoaded(Ok((id, table, detail))) => {
                // Discard batches that arrive after the overlay moved on.
                if self.design_pending != Some(id) {
                    return Task::none();
                }
                self.design_pending = None;
                let name = table
                    .path
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "tabla".to_string());
                self.designer =
                    Some(designer::DesignerState::from_table(id, table, detail));
                self.status = format!("diseñador: {name}");
                Task::none()
            }
            Message::DesignerLoaded(Err(e)) => {
                if let Some(id) = self.design_pending.take() {
                    if let Some(name) = self
                        .entries
                        .iter()
                        .find(|e| e.cfg.id == id)
                        .map(|e| e.cfg.name.clone())
                    {
                        self.status = format!("error al describir ({name}): {e}");
                    } else {
                        self.status = format!("error al describir la tabla: {e}");
                    }
                }
                Task::none()
            }
            Message::Design(msg) => {
                if let Some(d) = &mut self.designer {
                    d.apply(msg);
                }
                Task::none()
            }
            Message::DesignApply => {
                let Some(d) = &self.designer else {
                    return Task::none();
                };
                let engine = self.engine_of(d.connection);
                let stmts: Vec<String> = designer::preview(engine, d)
                    .into_iter()
                    .filter(|s| !s.trim_start().starts_with("--"))
                    .collect();
                if stmts.is_empty() {
                    self.status = "sin cambios que aplicar".to_string();
                    return Task::none();
                }
                let Some(session) = self.sessions.get(&d.connection).cloned() else {
                    self.status = "la conexión no está activa — selecciónala de nuevo".to_string();
                    return Task::none();
                };
                self.status = format!("aplicando {} sentencia(s)…", stmts.len());
                design_apply_task(self.rt.clone(), session.client.clone(), stmts)
            }
            Message::DesignApplied(Ok(n)) => {
                self.status = format!("✓ {n} sentencia(s) aplicadas");
                if let Some(d) = self.designer.take() {
                    let target = d.target();
                    self.invalidate_after_ddl(&target);
                    // Reload the parent listing so new tables show up.
                    if let Some(parent) = target.parent() {
                        self.expanded.remove(&parent);
                        self.tree.remove(&parent);
                        return self.expand(&parent);
                    }
                }
                Task::none()
            }
            Message::DesignApplied(Err(e)) => {
                self.status = format!("error al aplicar: {e}");
                Task::none()
            }
            Message::DesignClose => {
                self.designer = None;
                self.design_pending = None;
                Task::none()
            }
            Message::SyncOpen => {
                let Some((obj, kind)) = self.selected_object.clone() else {
                    self.status =
                        "selecciona la tabla de origen en el árbol".to_string();
                    return Task::none();
                };
                if kind != ObjectKind::Table {
                    self.status = "la sincronización necesita una tabla origen".to_string();
                    return Task::none();
                }
                let Some(src_session) = self.sessions.get(&obj.connection).cloned() else {
                    self.status = "la conexión de origen no está activa".to_string();
                    return Task::none();
                };
                let Some(dst) = self
                    .entries
                    .iter()
                    .find(|e| e.cfg.id != obj.connection && self.sessions.contains_key(&e.cfg.id))
                    .map(|e| e.cfg.id)
                else {
                    self.status =
                        "conecta otra base de datos para usarla como destino".to_string();
                    return Task::none();
                };
                let Some(dst_session) = self.sessions.get(&dst).cloned() else {
                    return Task::none();
                };
                let target = ObjectRef { connection: dst, path: obj.path.clone() };
                self.er = None;
                self.designer = None;
                self.design_pending = None;
                self.completion.close();
                self.sync_pending = Some(dst);
                self.status = "comparando estructura origen → destino…".to_string();
                sync_task(
                    self.rt.clone(),
                    src_session.client.clone(),
                    obj,
                    dst_session.client.clone(),
                    target,
                    dst,
                )
            }
            Message::SyncDest(dst) => {
                let Some(state) = &self.sync else {
                    return Task::none();
                };
                if dst == state.target_conn || self.sync_pending == Some(dst) {
                    return Task::none();
                }
                let Some(src_session) = self.sessions.get(&state.source.connection).cloned() else {
                    self.status = "la conexión de origen no está activa".to_string();
                    return Task::none();
                };
                let Some(dst_session) = self.sessions.get(&dst).cloned() else {
                    self.status = "esa conexión destino no está activa".to_string();
                    return Task::none();
                };
                let source = state.source.clone();
                let target = ObjectRef { connection: dst, path: source.path.clone() };
                self.sync_pending = Some(dst);
                self.status = "comparando estructura origen → destino…".to_string();
                sync_task(
                    self.rt.clone(),
                    src_session.client.clone(),
                    source,
                    dst_session.client.clone(),
                    target,
                    dst,
                )
            }
            Message::SyncLoaded(Ok((source, source_detail, dst, target, target_detail))) => {
                if self.sync_pending != Some(dst) {
                    return Task::none();
                }
                self.sync_pending = None;
                self.sync = Some(SyncState {
                    source,
                    source_detail,
                    target_conn: dst,
                    target,
                    target_detail,
                });
                self.status = if self
                    .sync
                    .as_ref()
                    .is_some_and(|s| s.target_detail.is_none())
                {
                    "tabla destino no existe: se creará completa".to_string()
                } else {
                    "estructura comparada: revisa el plan".to_string()
                };
                Task::none()
            }
            Message::SyncLoaded(Err(e)) => {
                if self.sync_pending.take().is_some() {
                    self.status = format!("error al comparar: {e}");
                }
                Task::none()
            }
            Message::SyncApply => {
                let Some(state) = &self.sync else {
                    return Task::none();
                };
                let engine = self.engine_of(state.target_conn);
                let stmts: Vec<String> = sync::plan(
                    engine,
                    &state.source_detail,
                    state.target_detail.as_ref(),
                    &state.target,
                )
                .statements
                .into_iter()
                .filter(|s| !s.trim_start().starts_with("--"))
                .collect();
                if stmts.is_empty() {
                    self.status = "el destino ya coincide con el origen".to_string();
                    return Task::none();
                }
                let Some(session) = self.sessions.get(&state.target_conn).cloned() else {
                    self.status = "la conexión destino no está activa".to_string();
                    return Task::none();
                };
                self.status = format!("aplicando {} sentencia(s) en el destino…", stmts.len());
                sync_apply_task(self.rt.clone(), session.client.clone(), stmts)
            }
            Message::SyncApplied(Ok(n)) => {
                self.status = format!("✓ {n} sentencia(s) aplicadas en el destino");
                if let Some(state) = self.sync.take() {
                    self.invalidate_after_ddl(&state.target);
                    if Some(state.target_conn) == self.active_id {
                        if let Some(parent) = state.target.parent() {
                            self.expanded.remove(&parent);
                            self.tree.remove(&parent);
                            return self.expand(&parent);
                        }
                    }
                }
                Task::none()
            }
            Message::SyncApplied(Err(e)) => {
                self.status = format!("error al migrar: {e}");
                Task::none()
            }
            Message::SyncClose => {
                self.sync = None;
                self.sync_pending = None;
                Task::none()
            }
            Message::CopyValues => self.copy_rows(false),
            Message::CopyWithHeaders => self.copy_rows(true),
            Message::Export => {
                if self.last_columns.is_empty() {
                    self.status = "no hay resultados para exportar".to_string();
                    return Task::none();
                }
                // The native dialog blocks; run it off-thread and resume via
                // a oneshot channel.
                let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
                std::thread::spawn(move || {
                    let mut dialog = rfd::FileDialog::new()
                        .set_file_name("resultado.csv")
                        .add_filter("CSV", &["csv"])
                        .add_filter("JSON", &["json"]);
                    if let Some(docs) = dirs::document_dir() {
                        dialog = dialog.set_directory(docs);
                    }
                    let _ = tx.send(dialog.save_file());
                });
                Task::perform(
                    async move { rx.await.unwrap_or(None) },
                    Message::ExportTo,
                )
            }
            Message::ExportTo(Some(path)) => {
                let is_json = path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"));
                let rows: Vec<&[CellValue]> = self
                    .grid
                    .store()
                    .rows()
                    .iter()
                    .map(|r| r.cells.as_slice())
                    .collect();
                let content = if is_json {
                    export::json(&self.last_columns, &rows)
                } else {
                    export::csv(&self.last_columns, &rows)
                };
                match std::fs::write(&path, content) {
                    Ok(()) => self.status = format!("exportado a {}", path.display()),
                    Err(e) => self.status = format!("error al exportar: {e}"),
                }
                Task::none()
            }
            Message::ExportTo(None) => {
                self.status = "exportación cancelada".to_string();
                Task::none()
            }
            Message::WindowOpened(id) => {
                self.window_id = Some(id);
                // Windows DWM bakes a caption-sized presentation offset at
                // creation (winit keeps WS_CAPTION for snap support while
                // spreading the client over the frame). Strip the frame
                // styles, then nudge the height so DWM re-lays the
                // composition out at the client origin.
                Task::batch([
                    enforce_title_frame(id),
                    Task::perform(
                        async {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        },
                        |_| Message::WinNudgeTitle(true),
                    ),
                ])
            }
            Message::WinNudgeTitle(up) => {
                let Some(id) = self.window_id else {
                    return Task::none();
                };
                if up {
                    Task::batch([
                        nudge_title_frame(id, 1),
                        Task::perform(
                            async {
                                tokio::time::sleep(std::time::Duration::from_millis(350)).await;
                            },
                            |_| Message::WinNudgeTitle(false),
                        ),
                    ])
                } else {
                    nudge_title_frame(id, -1)
                }
            }
            Message::WinFrameChecked => Task::none(),
            Message::WinEvent(ev) => match ev {
                iced::window::Event::Resized(_)
                | iced::window::Event::Focused
                | iced::window::Event::Moved(_) => match self.window_id {
                    Some(id) => enforce_title_frame(id),
                    None => Task::none(),
                },
                _ => Task::none(),
            },
            Message::WinResize(direction) => match self.window_id {
                Some(id) => iced::window::drag_resize(id, direction),
                None => Task::none(),
            },
            Message::WinDrag => {
                let Some(id) = self.window_id else {
                    return Task::none();
                };
                // Two presses inside the double-click interval maximize
                // instead of starting a second OS drag.
                let now = std::time::Instant::now();
                if let Some(last) = self.last_title_press
                    && now.duration_since(last) < std::time::Duration::from_millis(450)
                {
                    self.last_title_press = None;
                    return iced::window::toggle_maximize(id);
                }
                self.last_title_press = Some(now);
                iced::window::drag(id)
            }
            Message::WinMinimize => match self.window_id {
                Some(id) => iced::window::minimize(id, true),
                None => Task::none(),
            },
            Message::WinToggleMax => match self.window_id {
                Some(id) => iced::window::toggle_maximize(id),
                None => Task::none(),
            },
            Message::WinClose => match self.window_id {
                Some(id) => iced::window::close(id),
                None => Task::none(),
            },
            Message::WinSystemMenu => match self.window_id {
                Some(id) => iced::window::show_system_menu(id),
                None => Task::none(),
            },
        }
    }

    /// Copy the result grid as TSV: selected rows, or every row when the
    /// selection is empty. `headers` prepends the column-name line.
    fn copy_rows(&mut self, headers: bool) -> Task<Message> {
        let tsv = {
            let selection = self.grid.selection();
            let rows = self.grid.store().rows();
            let mut lines: Vec<String> = Vec::new();
            if headers && !self.last_columns.is_empty() {
                lines.push(
                    self.last_columns
                        .iter()
                        .map(|c| sanitize(&c.name))
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
            }
            let mut n = 0usize;
            for row in rows {
                if !selection.is_empty() && !selection.contains(&row.idx) {
                    continue;
                }
                lines.push(
                    row.cells
                        .iter()
                        .map(|c| sanitize(&c.as_display()))
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
                n += 1;
            }
            (lines, n)
        };
        let (lines, n) = tsv;
        if n == 0 && lines.is_empty() {
            self.status = "no hay resultados para copiar".to_string();
            return Task::none();
        }
        self.status = format!(
            "copiadas {n} fila(s){} al portapapeles",
            if headers { " con encabezados" } else { "" }
        );
        iced::clipboard::write(lines.join("\r\n"))
    }

    /// Save the live editor/result/connection fields into `tabs[active_tab]`
    /// (the grid is moved out and replaced with an empty one).
    fn stash_tab(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        tab.content = self.editor.content().into();
        tab.grid = std::mem::replace(&mut self.grid, grid::empty());
        tab.connection = self.active_id;
        tab.executed = self.executed;
        tab.last_row_count = self.last_row_count;
        tab.last_columns = std::mem::take(&mut self.last_columns);
        tab.query_context = self.query_context.clone();
    }

    /// Restore `tabs[index]` into the live fields and refill its grid.
    fn load_tab(&mut self, index: usize) -> Task<Message> {
        let Some(tab) = self.tabs.get_mut(index) else {
            return Task::none();
        };
        self.active_id = tab.connection;
        self.root = tab.connection.map(ObjectRef::root);
        self.selected = self
            .active_id
            .and_then(|id| self.entries.iter().position(|e| e.cfg.id == id));
        self.executed = tab.executed;
        self.last_row_count = tab.last_row_count;
        self.last_columns = std::mem::take(&mut tab.last_columns);
        self.query_context = tab.query_context.clone();
        let content = tab.content.clone();
        self.grid = std::mem::replace(&mut tab.grid, grid::empty());
        // Refill the restored grid with its last query (set_spec resets the
        // window and returns the initial fetch; `fetch_task` is private).
        let spec = self.grid.spec().clone();
        let refill = self.grid.set_spec(spec).map(Message::Grid);
        self.completion.close();
        self.completion.dictionary = self
            .active_id
            .and_then(|id| self.completion_words.get(&id))
            .cloned()
            .unwrap_or_default();
        let edit = self.editor.reset(&content).map(Message::Editor);
        Task::batch([edit, refill])
    }

    /// Result of a finished statement: goes live when it belongs to the
    /// active tab, otherwise it is parked in its tab (refilled on switch).
    fn apply_result(&mut self, tab_index: usize, result: QueryResult) -> Task<Message> {
        let ms = result.elapsed.as_millis();
        let status = if result.is_rows() {
            format!(
                "{} filas{} ({ms} ms)",
                result.row_count(),
                if result.truncated {
                    format!(" (truncadas en {MAX_ROWS})")
                } else {
                    String::new()
                }
            )
        } else {
            format!("{} afectada(s) ({ms} ms)", result.affected.unwrap_or(0))
        };
        let executed = result.is_rows();
        let columns = result.columns.clone();
        let row_count = result.row_count();
        if tab_index == self.active_tab {
            self.executed = true;
            self.status = status;
            self.last_columns = columns;
            self.last_row_count = row_count;
            let (grid, init) = grid::build(result);
            self.grid = grid;
            init.map(Message::Grid)
        } else if let Some(tab) = self.tabs.get_mut(tab_index) {
            let (grid, _init) = grid::build(result);
            tab.grid = grid;
            tab.executed = executed;
            tab.last_columns = columns;
            tab.last_row_count = row_count;
            Task::none()
        } else {
            // Tab was closed while the statement ran: drop the result.
            Task::none()
        }
    }

    /// Select a connection (tree row or legacy list): rebind the active
    /// tab to it, refresh side panels and connect when idle.
    fn select_connection(&mut self, i: usize) -> Task<Message> {
        let Some(entry) = self.entries.get(i) else {
            return Task::none();
        };
        self.selected = Some(i);
        let (name, state, id) = (
            entry.cfg.name.clone(),
            entry.state.clone(),
            entry.cfg.id,
        );
        // Rebind the ACTIVE TAB to this connection: the sidebar picks
        // the database that the current workspace talks to.
        self.active_id = Some(id);
        self.root = Some(ObjectRef::root(id));
        self.query_context = None;
        self.selected_object = None;
        self.sync = None;
        self.sync_pending = None;
        self.completion.close();
        self.completion.dictionary = self
            .completion_words
            .get(&id)
            .cloned()
            .unwrap_or_default();
        match state {
            ConnState::Connected => {
                self.status = format!("conectada: {name}");
                Task::none()
            }
            ConnState::Connecting => {
                self.status = format!("conectando… {name}");
                Task::none()
            }
            ConnState::Idle => {
                self.status = format!("conectando: {name}");
                let cfg = self.entries[i].cfg.clone();
                self.entries[i].state = ConnState::Connecting;
                connect_task(self.rt.clone(), cfg)
            }
            ConnState::Error(err) => {
                self.status = format!("reintentando {name}: {err}");
                let cfg = self.entries[i].cfg.clone();
                self.entries[i].state = ConnState::Connecting;
                connect_task(self.rt.clone(), cfg)
            }
        }
    }

    /// Engine of a connection id (defaults to SQLite for display paths that
    /// run before any entry is loaded).
    fn engine_of(&self, id: ConnectionId) -> Engine {
        self.entries
            .iter()
            .find(|e| e.cfg.id == id)
            .map(|e| e.cfg.engine)
            .unwrap_or(Engine::Sqlite)
    }

    /// Drop cached children/expansion for `target` and everything under it
    /// so the navigator refetches after a DDL apply.
    fn invalidate_after_ddl(&mut self, target: &ObjectRef) {
        self.tree.retain(|k, _| !is_under(target, k));
        self.expanded.retain(|k| !is_under(target, k));
    }

    /// Toggle (or lazily load) the children of a navigator node.
    fn expand(&mut self, node: &ObjectRef) -> Task<Message> {
        if self.expanded.contains(node) {
            self.expanded.remove(node);
            return Task::none();
        }
        self.expanded.insert(node.clone());
        if self.tree.contains_key(node) {
            return Task::none();
        }
        self.load_children(node)
    }

    /// Fetch `node`'s children right now (cache is already cleared by the
    /// caller): used by right-click "Actualizar" on nodes and roots.
    fn reload_children(&mut self, node: &ObjectRef) -> Task<Message> {
        self.tree.remove(node);
        self.expanded.insert(node.clone());
        self.load_children(node)
    }

    /// Background listing for one node; every node carries its connection
    /// (`child()` propagates it), so multi-DB tabs each expand their own
    /// tree via the shared map.
    fn load_children(&self, node: &ObjectRef) -> Task<Message> {
        let Some(session) = self.sessions.get(&node.connection).cloned() else {
            return Task::none();
        };
        let client = session.client.clone();
        let rt = self.rt.clone();
        let reply = node.clone();
        let target = node.clone();
        Task::perform(
            async move {
                rt.spawn(async move {
                    client.list_children(&target).await.map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())?
            },
            move |res| Message::ChildrenLoaded(reply.clone(), res),
        )
    }

    /// Starter query for a clicked table/view/collection node.
    fn query_for(&self, obj: &ObjectRef, kind: ObjectKind) -> String {
        let engine = self
            .entries
            .iter()
            .find(|e| Some(e.cfg.id) == self.active_id)
            .map(|e| e.cfg.engine)
            .unwrap_or(Engine::Sqlite);
        query_for_engine(engine, obj, kind)
    }

    /// Centered muted message filling the ER body area.
    fn er_placeholder(msg: String) -> Element<'static, Message> {
        container(text(msg).size(14).color(theme::muted()))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// Workspace shown while the ER diagram is open: a header strip with
    /// reload/close actions over the scrollable diagram canvas (or a
    /// loading/empty placeholder).
    fn er_view<'a>(&'a self, er: &'a ErState) -> Element<'a, Message> {
        let conn_name = self
            .entries
            .iter()
            .find(|e| e.cfg.id == er.connection)
            .map(|e| e.cfg.name.clone())
            .unwrap_or_else(|| "conexión".to_string());
        let strip = container(
            row![
                text("Diagrama ER").size(13),
                text(conn_name).size(12).color(theme::muted()),
                container(
                    row![
                        button(text("↻ Recargar").size(12))
                            .on_press(Message::ErOpen)
                            .style(theme::flat),
                        button(text("✕ Volver").size(12))
                            .on_press(Message::ErClose)
                            .style(theme::flat),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
        .style(theme::chrome);

        let body: Element<'_, Message> = if er.loading {
            Self::er_placeholder("Cargando tablas…".to_string())
        } else if let Some(diagram) = &er.diagram {
            let (w, h) = diagram.size;
            scrollable(
                container(
                    iced::widget::canvas(diagram)
                        .width(Length::Fixed(w + 20.0))
                        .height(Length::Fixed(h + 20.0)),
                )
                .padding(10),
            )
            .height(Length::Fill)
            .into()
        } else if er.failed.is_empty() {
            Self::er_placeholder("Sin tablas que mostrar en esta conexión".to_string())
        } else {
            Self::er_placeholder(format!("Sin tablas — {}", er.failed.join(" · ")))
        };

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(body)
            .width(Length::Fill)
            .into()
    }

    /// Workspace shown while the table designer is open: header strip with
    /// apply/close actions over the column grid + SQL preview.
    fn designer_view<'a>(&'a self, d: &'a designer::DesignerState) -> Element<'a, Message> {
        let engine = self.engine_of(d.connection);
        let conn_name = self
            .entries
            .iter()
            .find(|e| e.cfg.id == d.connection)
            .map(|e| e.cfg.name.clone())
            .unwrap_or_else(|| "conexión".to_string());
        let title = match &d.table {
            Some(t) => format!(
                "Diseñador: {}",
                t.path.last().cloned().unwrap_or_default()
            ),
            None => "Diseñador: nueva tabla".to_string(),
        };
        let strip = container(
            row![
                text(title).size(13),
                text(conn_name).size(12).color(theme::muted()),
                container(
                    row![
                        button(text("✓ Aplicar").size(12))
                            .on_press(Message::DesignApply)
                            .style(theme::solid),
                        button(text("✕ Volver").size(12))
                            .on_press(Message::DesignClose)
                            .style(theme::flat),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
        .style(theme::chrome);

        let body: Element<'_, Message> = scrollable(
            container(designer::view(d, engine).map(Message::Design)).padding(10),
        )
        .height(Length::Fill)
        .into();

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(body)
            .width(Length::Fill)
            .into()
    }

    /// Workspace shown while a structure sync is open: destination picker
    /// over the action grid + migration SQL preview.
    fn sync_view<'a>(&'a self, s: &'a SyncState) -> Element<'a, Message> {
        let dst_name = self
            .entries
            .iter()
            .find(|e| e.cfg.id == s.target_conn)
            .map(|e| e.cfg.name.clone())
            .unwrap_or_else(|| "destino".to_string());
        let table = s.source.path.last().cloned().unwrap_or_default();

        let mut dest_picker = row![text("Destino:").size(11).color(theme::muted())].spacing(6);
        for e in &self.entries {
            if e.cfg.id == s.source.connection || !self.sessions.contains_key(&e.cfg.id) {
                continue;
            }
            let active = e.cfg.id == s.target_conn && self.sync_pending.is_none();
            dest_picker = dest_picker.push(
                button(text(e.cfg.name.clone()).size(12))
                    .padding([4, 8])
                    .style(if active { theme::solid } else { theme::flat })
                    .on_press(Message::SyncDest(e.cfg.id)),
            );
        }

        let strip = container(
            row![
                text(format!("Sincronizar: {table}")).size(13),
                dest_picker,
                container(
                    row![
                        button(text("✓ Aplicar").size(12))
                            .on_press_maybe(self.sync_pending.is_none().then_some(Message::SyncApply))
                            .style(theme::solid),
                        button(text("✕ Volver").size(12))
                            .on_press(Message::SyncClose)
                            .style(theme::flat),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
        .style(theme::chrome);

        let body: Element<'_, Message> = if self.sync_pending.is_some() {
            container(
                text("comparando estructura origen → destino…")
                    .size(13)
                    .color(theme::muted()),
            )
            .padding(16)
            .into()
        } else {
            let engine = self.engine_of(s.target_conn);
            let plan = sync::plan(
                engine,
                &s.source_detail,
                s.target_detail.as_ref(),
                &s.target,
            );

            let mut header = row![].spacing(6).align_y(iced::Alignment::Center);
            for (label, w) in [
                ("Columna", 150.0),
                ("Acción", 110.0),
                ("Origen", 170.0),
                ("Destino", 170.0),
            ] {
                header = header.push(
                    text(label)
                        .size(11)
                        .color(theme::muted())
                        .width(Length::Fixed(w)),
                );
            }
            header = header.push(text(" ").size(11).width(Length::Fill));

            let mut rows = Column::new().push(header).spacing(4);
            for row_data in &plan.rows {
                let action = text(row_data.action.label())
                    .size(13)
                    .width(Length::Fixed(110.0));
                let action = if row_data.action == sync::Action::Keep {
                    action.color(theme::muted())
                } else {
                    action
                };
                rows = rows.push(
                    row![
                        text(row_data.column.clone())
                            .size(13)
                            .width(Length::Fixed(150.0)),
                        action,
                        text(row_data.source.clone())
                            .size(13)
                            .color(theme::muted())
                            .width(Length::Fixed(170.0)),
                        text(row_data.target.clone())
                            .size(13)
                            .color(theme::muted())
                            .width(Length::Fixed(170.0)),
                        text(" ").size(13).width(Length::Fill),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                );
            }

            let runnable: Vec<&String> = plan
                .statements
                .iter()
                .filter(|st| !st.trim_start().starts_with("--"))
                .collect();
            let sql = if plan.statements.is_empty() {
                "-- sin diferencias: el destino ya coincide".to_string()
            } else {
                plan.statements.join("\n")
            };
            let preview_pane = container(
                scrollable(text(sql).size(12).font(iced::Font::MONOSPACE))
                    .height(Length::Fixed(170.0)),
            )
            .padding(10)
            .style(theme::workspace);

            scrollable(
                container(
                    Column::new()
                        .push(rows)
                        .push(
                            text(format!(
                                "SQL a aplicar en {} ({} sentencia(s) ejecutables)",
                                dst_name,
                                runnable.len()
                            ))
                            .size(11)
                            .color(theme::muted()),
                        )
                        .push(preview_pane)
                        .spacing(8),
                )
                .padding(10),
            )
            .height(Length::Fill)
            .into()
        };

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(body)
            .width(Length::Fill)
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        // ── custom title bar: brand + actions + window controls ─────────
        // Frameless window: the bar replaces the native title bar, so it is
        // also the drag region (MouseArea) — the widgets inside must capture
        // their own clicks. Fixed height: the vertical rule is a Fill child
        // and would otherwise stretch the hug-height band unpredictably.
        let window_controls = row![
            button(text("–").size(14))
                .width(Length::Fixed(46.0))
                .height(Length::Fill)
                .on_press(Message::WinMinimize)
                .style(theme::win_control),
            button(text("□").size(13))
                .width(Length::Fixed(46.0))
                .height(Length::Fill)
                .on_press(Message::WinToggleMax)
                .style(theme::win_control),
            button(text("✕").size(13))
                .width(Length::Fixed(46.0))
                .height(Length::Fill)
                .on_press(Message::WinClose)
                .style(theme::win_close),
        ]
        .spacing(0)
        .height(Length::Fill)
        .align_y(iced::Alignment::Center);

        let er_msg = self.active_id.is_some().then_some(Message::ErOpen);
        let theme_icon = if self.dark {
            icons::Icon::Moon
        } else {
            icons::Icon::Sun
        };
        let toolbar = row![
            text("jorgenio dbstudio").size(14),
            tool_btn(icons::Icon::Add, "Conexión", Some(Message::OpenForm), false),
            tool_btn(icons::Icon::Query, "Nueva consulta", Some(Message::NewTab), false),
            tool_btn(icons::Icon::Run, "Ejecutar", Some(Message::Run), true),
            tool_btn(
                icons::Icon::NewTable,
                "Nueva tabla",
                (self.active_id.is_some()
                    && self.engine_of(self.active_id.unwrap()) != Engine::Mongodb)
                    .then_some(Message::DesignNew),
                false
            ),
            tool_btn(
                icons::Icon::Design,
                "Diseñador",
                self.selected_object
                    .as_ref()
                    .is_some_and(|(o, k)| *k == ObjectKind::Table && self.sessions.contains_key(&o.connection))
                    .then_some(Message::DesignSelected),
                false
            ),
            tool_btn(
                icons::Icon::Sync,
                "Sincronizar",
                self.selected_object.as_ref().is_some_and(|(o, k)| {
                    *k == ObjectKind::Table
                        && self.sessions.contains_key(&o.connection)
                        && self
                            .sessions
                            .keys()
                            .any(|id| *id != o.connection)
                })
                .then_some(Message::SyncOpen),
                false
            ),
            tool_btn(icons::Icon::Er, "Diagrama ER", er_msg, false),
            tool_btn(theme_icon, "Tema", Some(Message::ToggleTheme), false),
            rule::vertical(1).style(theme::hairline),
            container(auto_complete(
                &self.search,
                "Buscar conexión…",
                Message::SearchInput,
                Message::SearchSubmit,
                Some(Message::SearchSelect),
            ))
            .width(Length::Fixed(280.0))
            .padding(2),
            container(window_controls)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::End),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let title_bar = MouseArea::new(
            container(toolbar)
                .width(Length::Fill)
                .height(Length::Fixed(56.0))
                .padding(iced::Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                    left: 8.0,
                })
                .style(theme::chrome),
        )
        .on_press(Message::WinDrag)
        .on_right_press(Message::WinSystemMenu);

        // ── navigator: connections + lazy object tree ───────────────────
        let mut navigator: Column<'_, Message> = Column::new().spacing(2);
        let mut conn_header = row![text("CONEXIONES").size(11).color(theme::muted())]
            .align_y(iced::Alignment::Center);
        if self.selected.is_some() && self.form.is_none() {
            conn_header = conn_header.push(
                container(
                    row![
                        button(text("✎ Editar").size(11))
                            .on_press(Message::EditSelected)
                            .style(theme::flat),
                        button(text("✕ Eliminar").size(11))
                            .on_press(Message::DeleteSelected)
                            .style(theme::flat),
                    ]
                    .spacing(4),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );
        }
        navigator = navigator.push(conn_header);
        navigator = navigator.push(
            button(
                row![
                    icons::svg(icons::Icon::Add, 13.0),
                    text("+ Nueva conexión").size(13),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .on_press(Message::OpenForm)
            .style(theme::flat),
        );
        if let Some(form) = &self.form {
            navigator = navigator.push(
                container(form::view(form, self.editing.is_some()).map(Message::Form))
                    .padding(6)
                    .style(theme::workspace),
            );
        } else {
            // ── unified tree: "Mis conexiones" → connections → objects ──
            let filter = self.tree_filter.trim().to_string();
            let filter_active = !filter.is_empty();
            let root_open = self.expanded_root || filter_active;
            let root_marker = if root_open { "▾" } else { "▸" };
            navigator = navigator.push(
                button(
                    container(
                        row![
                            text(root_marker).size(11).color(theme::muted()),
                            icons::svg(icons::Icon::Connection, 14.0),
                            text("Mis conexiones").size(13),
                        ]
                        .spacing(4)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding(iced::Padding {
                        top: 2.0,
                        right: 6.0,
                        bottom: 2.0,
                        left: 6.0,
                    }),
                )
                .width(Length::Fill)
                .style(theme::flat)
                .on_press(Message::TreeRootToggle),
            );
            if root_open {
                for (i, entry) in self.entries.iter().enumerate() {
                    let conn_root = ObjectRef::root(entry.cfg.id);
                    if filter_active
                        && !node_matches(&entry.cfg.name, &filter)
                        && !subtree_matches(self, &conn_root, &filter)
                    {
                        continue;
                    }
                    let (marker, marker_color) = match &entry.state {
                        ConnState::Connected => ("●", theme::engine_color(entry.cfg.engine)),
                        ConnState::Connecting => ("◐", theme::engine_color(entry.cfg.engine)),
                        ConnState::Idle => ("○", theme::muted()),
                        ConnState::Error(_) => ("✕", iced::color!(0xd93025)),
                    };
                    let open = self.expanded.contains(&conn_root);
                    let arrow = if open { "▾" } else { "▸" };
                    let label = row![
                        text(arrow).size(11).color(theme::muted()),
                        icons::svg(icons::Icon::Connection, 14.0),
                        text(marker).size(11).color(marker_color),
                        text(entry.cfg.name.clone()).size(13),
                    ]
                    .spacing(4)
                    .align_y(iced::Alignment::Center);
                    let row_style: fn(&Theme, button::Status) -> button::Style =
                        if self.selected == Some(i) {
                            theme::row_selected
                        } else {
                            theme::flat
                        };
                    navigator = navigator.push(
                        MouseArea::new(
                            button(
                                container(label)
                                    .padding(iced::Padding {
                                        top: 2.0,
                                        right: 6.0,
                                        bottom: 2.0,
                                        left: 20.0,
                                    }),
                            )
                            .width(Length::Fill)
                            .style(row_style)
                            .on_press(Message::TreeToggleConn(entry.cfg.id)),
                        )
                        .on_right_press(Message::ContextOpen(MenuTarget::Conn(
                            entry.cfg.id,
                        ))),
                    );
                    if let Some(menu) = &self.context_menu {
                        if matches!(
                            &menu.target,
                            MenuTarget::Conn(id) if *id == entry.cfg.id
                        ) {
                            navigator = navigator.push(context_menu_view(self, menu));
                        }
                    }
                    if open || filter_active {
                        navigator = push_tree(self, &conn_root, 1, navigator);
                    }
                }
            }
        }
        let conn_count = self.entries.len();
        let filter_active = !self.tree_filter.trim().is_empty();
        let mut filter_row = row![
            text_input("Buscar objeto…", &self.tree_filter)
                .on_input(Message::TreeFilter)
                .size(12)
                .padding([5, 7]),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        if filter_active {
            filter_row = filter_row.push(
                button(text("✕").size(11))
                    .padding([3, 7])
                    .style(theme::flat)
                    .on_press(Message::TreeFilter(String::new())),
            );
        }
        let navigator = container(
            Column::new()
                .push(scrollable(navigator).height(Length::Fill))
                .push(filter_row)
                .push(
                    text(if conn_count == 1 {
                        "1 conexión".to_string()
                    } else {
                        format!("{conn_count} conexiones")
                    })
                    .size(11)
                    .color(theme::muted()),
                )
                .spacing(6),
        )
        .width(Length::Fixed(260.0))
        .height(Length::Fill)
        .padding(6)
        .style(theme::navigator);

        // ── editor pane with its tab strip ──────────────────────────────
        let mut tab_row = row![].spacing(4).align_y(iced::Alignment::Center);
        for (i, tab) in self.tabs.iter().enumerate() {
            let style: fn(&Theme, button::Status) -> button::Style =
                if i == self.active_tab {
                    theme::tab_active
                } else {
                    theme::tab_inactive
                };
            tab_row = tab_row.push(
                row![
                    button(
                        row![
                            icons::svg(icons::Icon::Query, 13.0),
                            text(format!(" {} ", tab.title)).size(13),
                        ]
                        .spacing(4)
                        .align_y(iced::Alignment::Center)
                    )
                    .style(style)
                    .on_press(Message::SelectTab(i)),
                    button(text("✕").size(10))
                        .style(theme::flat)
                        .on_press(Message::CloseTab(i)),
                ]
                .spacing(2)
                .align_y(iced::Alignment::Center),
            );
        }
        tab_row = tab_row.push(
            button(text(" + ").size(14))
                .style(theme::flat)
                .on_press(Message::NewTab),
        );
        let editor_tabs = container(tab_row)
            .width(Length::Fill)
            .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
            .style(theme::chrome);
        // The completion popup floats over the editor (bottom-left); when
        // closed the overlay is an inert empty space.
        let editor_layers: Vec<Element<Message>> = vec![
            self.editor.view().map(Message::Editor).into(),
            if self.completion.open {
                completion_popup(&self.completion)
            } else {
                Element::from(Space::new())
            },
        ];
        let editor = container(stack(editor_layers))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(6)
            .style(theme::workspace);

        // ── result strip: tab + row count + copy actions ────────────────
        let has_results = !self.last_columns.is_empty();
        let result_tab_style: fn(&Theme, button::Status) -> button::Style =
            if self.executed {
                theme::tab_active
            } else {
                theme::tab_inactive
            };
        let result_strip = container(
            row![
                button(text("  Resultado 1  ").size(13)).style(result_tab_style),
                text(format!("{} filas", self.last_row_count))
                    .size(12)
                    .color(theme::muted()),
                container(
                    row![
                        button(text("Copiar").size(12))
                            .on_press_maybe(has_results.then_some(Message::CopyValues))
                            .style(theme::flat),
                        button(text("Copiar c/nombres").size(12))
                            .on_press_maybe(has_results.then_some(Message::CopyWithHeaders))
                            .style(theme::flat),
                        button(text("Exportar…").size(12))
                            .on_press_maybe(has_results.then_some(Message::Export))
                            .style(theme::flat),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
        .style(theme::chrome);
        let grid = container(self.grid.view().map(Message::Grid))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::workspace);

        // The sync/designer/ER overlays replace the editor + results while open.
        let center: Element<'_, Message> = match (&self.sync, &self.designer, &self.er) {
            (Some(s), _, _) => self.sync_view(s),
            (None, Some(d), _) => self.designer_view(d),
            (None, None, Some(er)) => self.er_view(er),
            (None, None, None) => Column::new()
                .push(editor_tabs)
                .push(editor.height(Length::FillPortion(2)))
                .push(rule::horizontal(1).style(theme::hairline))
                .push(result_strip)
                .push(grid.height(Length::FillPortion(3)))
                .width(Length::Fill)
                .into(),
        };

        let main = row![navigator, center];

        // ── status bar: message left, live connection right ─────────────
        let (conn_label, conn_color) = self
            .active_id
            .and_then(|id| self.entries.iter().find(|e| e.cfg.id == id))
            .map(|e| {
                (
                    format!("● {} · {}", e.cfg.engine, e.cfg.name),
                    theme::engine_color(e.cfg.engine),
                )
            })
            .unwrap_or_else(|| ("○ sin conexión".to_string(), theme::muted()));
        let status_bar = container(
            row![
                text(format!(" {}", self.status)).size(12),
                container(
                    row![text(conn_label).size(12).color(conn_color)]
                        .align_y(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            ]
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([4.0, 8.0])
        .style(theme::chrome);

        let app: Element<Message> = Column::new()
            .push(title_bar)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(main.height(Length::Fill))
            .push(rule::horizontal(1).style(theme::hairline))
            .push(status_bar)
            .into();

        // Frameless windows lose the OS resize borders: lay invisible
        // edge/corner zones on top (same hit pattern as native frames).
        stack(vec![
            app,
            resize_edge_zone(iced::window::Direction::North, true, true),
            resize_edge_zone(iced::window::Direction::South, true, false),
            resize_edge_zone(iced::window::Direction::West, false, true),
            resize_edge_zone(iced::window::Direction::East, false, false),
            resize_corner_zone(
                iced::window::Direction::NorthWest,
                iced::Alignment::Start,
                iced::Alignment::Start,
            ),
            resize_corner_zone(
                iced::window::Direction::NorthEast,
                iced::Alignment::End,
                iced::Alignment::Start,
            ),
            resize_corner_zone(
                iced::window::Direction::SouthWest,
                iced::Alignment::Start,
                iced::Alignment::End,
            ),
            resize_corner_zone(
                iced::window::Direction::SouthEast,
                iced::Alignment::End,
                iced::Alignment::End,
            ),
        ])
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let keys = iced::keyboard::listen().filter_map(|event| {
            let iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat: false,
                ..
            } = event
            else {
                return None;
            };
            let control = modifiers.control() && !modifiers.alt();
            match key {
                // F5 alone runs the statement (the editor declines it).
                iced::keyboard::Key::Named(iced::keyboard::key::Named::F5)
                    if modifiers.is_empty() =>
                {
                    Some(Message::Run)
                }
                // Ctrl+C reaches here only when no widget captured it, i.e.
                // when the editor is not focused (it owns Ctrl+C itself).
                iced::keyboard::Key::Character(ref c)
                    if control && !modifiers.shift() && c.as_ref() == "c" =>
                {
                    Some(Message::CopyValues)
                }
                iced::keyboard::Key::Character(ref c)
                    if control && modifiers.shift() && c.as_ref() == "c" =>
                {
                    Some(Message::CopyWithHeaders)
                }
                // Delete removes the selected sidebar connection (captured by
                // the editor while it has focus, so typing stays safe).
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Delete) => {
                    Some(Message::DeleteSelected)
                }
                // Ctrl+Space opens the completion popup (the editor declines
                // the combo, so it reaches this global listener).
                iced::keyboard::Key::Character(ref c)
                    if control && !modifiers.shift() && c.as_ref() == " " =>
                {
                    Some(Message::CompletionTrigger)
                }
                // Esc closes the popup (same reasoning as F5: declined by the
                // editor, seen here when the popup is open).
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
                    if modifiers.is_empty() =>
                {
                    Some(Message::CompletionClose)
                }
                _ => None,
            }
        });
        // The custom title bar needs the window id for drag/min/close; the
        // window events keep the frameless style asserted after resizes.
        let window = iced::window::open_events().map(Message::WindowOpened);
        let events = iced::window::events().map(|(_id, ev)| Message::WinEvent(ev));
        Subscription::batch([keys, window, events])
    }

    fn theme(&self) -> Theme {
        theme::app_theme(self.dark)
    }
}

/// Flatten a cell/label into a single-line TSV field.
fn sanitize(s: &str) -> String {
    if !s.contains(['\t', '\r', '\n']) {
        return s.to_string();
    }
    s.chars()
        .map(|ch| match ch {
            '\t' | '\r' | '\n' => ' ',
            other => other,
        })
        .collect()
}

/// Starter query for `obj`, dialect-aware. MongoDB nodes produce an Extended
/// JSON `find` command instead of SQL.
fn query_for_engine(engine: Engine, obj: &ObjectRef, kind: ObjectKind) -> String {
    let last = obj.path.last().map(String::as_str).unwrap_or("");

    if kind == ObjectKind::Collection {
        let esc = last.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("{{\n  \"find\": \"{esc}\",\n  \"filter\": {{}},\n  \"limit\": 100\n}}");
    }

    let from = match engine {
        // The first segment is the file's display label, not a schema.
        Engine::Sqlite => quote_ident(last, engine),
        // A session is bound to its database: the db segment would mean a
        // cross-database reference (rejected by PostgreSQL).
        Engine::Postgres | Engine::Oracle | Engine::Mongodb => obj
            .path
            .iter()
            .skip(1)
            .map(|s| quote_ident(s, engine))
            .collect::<Vec<_>>()
            .join("."),
        // [db, table] → db.table keeps browsing other databases portable.
        Engine::Mysql | Engine::MariaDb => obj
            .path
            .iter()
            .map(|s| quote_ident(s, engine))
            .collect::<Vec<_>>()
            .join("."),
        // [db, table] → db..table resolves the default schema cross-database.
        Engine::SqlServer if obj.path.len() == 2 => format!(
            "{}..{}",
            quote_ident(&obj.path[0], engine),
            quote_ident(&obj.path[1], engine)
        ),
        Engine::SqlServer => obj
            .path
            .iter()
            .skip(1)
            .map(|s| quote_ident(s, engine))
            .collect::<Vec<_>>()
            .join("."),
    };

    if engine == Engine::SqlServer {
        format!("SELECT TOP 100 * FROM {from};")
    } else {
        format!("SELECT * FROM {from} LIMIT 100;")
    }
}

/// Quote an identifier with the dialect's delimiter.
fn quote_ident(seg: &str, engine: Engine) -> String {
    match engine {
        Engine::Mysql | Engine::MariaDb => format!("`{}`", seg.replace('`', "``")),
        Engine::SqlServer => format!("[{}]", seg.replace(']', "]]")),
        _ => format!("\"{}\"", seg.replace('"', "\"\"")),
    }
}

/// `DROP TABLE/VIEW` for a tree object, qualified like `query_for_engine`
/// does (SQLite is bare; MySQL gets `db.table`, Postgres `schema.table`).
/// Returns `None` for kinds that cannot be dropped (collections, columns…).
fn drop_statement(engine: Engine, kind: ObjectKind, obj: &ObjectRef) -> Option<String> {
    let keyword = match kind {
        ObjectKind::Table => "DROP TABLE",
        ObjectKind::View => "DROP VIEW",
        _ => return None,
    };
    let name = obj.path.last()?;
    let quoted = quote_ident(name, engine);
    let full = if engine == Engine::Sqlite {
        quoted
    } else {
        match obj.path.get(obj.path.len().wrapping_sub(2)) {
            Some(seg) => format!("{}.{}", quote_ident(seg, engine), quoted),
            None => quoted,
        }
    };
    Some(format!("{keyword} {full};"))
}

/// Toolbar button in the Navicat style: big icon above a small caption.
/// `msg = None` renders the button disabled (greyed out).
fn tool_btn(
    icon: icons::Icon,
    label: &'static str,
    msg: Option<Message>,
    primary: bool,
) -> Element<'static, Message> {
    let inner = Column::new()
        .push(icons::svg(icon, 20.0))
        .push(text(label).size(10))
        .spacing(3)
        .align_x(iced::Alignment::Center);
    let b = button(inner)
        .padding(iced::Padding {
            top: 4.0,
            right: 9.0,
            bottom: 3.0,
            left: 9.0,
        })
        .style(if primary { theme::solid } else { theme::flat });
    match msg {
        Some(m) => b.on_press(m).into(),
        None => b.into(),
    }
}

/// One row of the navigator context menu: icon + caption, full width.
fn menu_item(
    icon: icons::Icon,
    label: &'static str,
    act: ContextAct,
) -> Element<'static, Message> {
    button(
        row![icons::svg(icon, 14.0), text(label).size(12)]
            .spacing(7)
            .align_y(iced::Alignment::Center),
    )
    .padding(iced::Padding {
        top: 4.0,
        right: 8.0,
        bottom: 4.0,
        left: 8.0,
    })
    .width(Length::Fill)
    .style(theme::flat)
    .on_press(Message::ContextAct(act))
    .into()
}

/// Context menu panel, rendered right below the row it belongs to.
/// The `confirm` state swaps the items for the destructive second step
/// ("¿Eliminar…?"), so a DROP never fires from a single click.
fn context_menu_view<'a>(app: &'a App, menu: &'a ContextMenu) -> Element<'a, Message> {
    let title: String = match &menu.target {
        MenuTarget::Conn(id) => app
            .entries
            .iter()
            .find(|e| e.cfg.id == *id)
            .map(|e| e.cfg.name.clone())
            .unwrap_or_else(|| "conexión".to_string()),
        MenuTarget::Node(obj, _) => obj.path.last().cloned().unwrap_or_default(),
    };
    let header = row![
        text(title.clone()).size(11).color(theme::muted()),
        Space::new().width(Length::Fill),
        button(text("✕").size(11))
            .padding([2.0, 7.0])
            .style(theme::flat)
            .on_press(Message::ContextClose),
    ]
    .align_y(iced::Alignment::Center);

    let mut body = Column::new().spacing(1).push(header);
    if menu.confirm {
        body = body
            .push(text(format!("¿Eliminar «{title}»?")).size(12))
            .push(
                text("Esta acción no se puede deshacer.")
                    .size(11)
                    .color(theme::muted()),
            )
            .push(menu_item(
                icons::Icon::Delete,
                "Sí, eliminar",
                ContextAct::ConfirmDelete,
            ))
            .push(menu_item(icons::Icon::Close, "Cancelar", ContextAct::Cancel));
    } else {
        match &menu.target {
            MenuTarget::Conn(_) => {
                body = body
                    .push(menu_item(
                        icons::Icon::Design,
                        "Editar conexión…",
                        ContextAct::EditConn,
                    ))
                    .push(menu_item(
                        icons::Icon::Refresh,
                        "Actualizar",
                        ContextAct::Refresh,
                    ))
                    .push(menu_item(
                        icons::Icon::NewTable,
                        "Nueva tabla…",
                        ContextAct::NewTable,
                    ))
                    .push(menu_item(
                        icons::Icon::Copy,
                        "Copiar nombre",
                        ContextAct::CopyName,
                    ))
                    .push(menu_item(
                        icons::Icon::Delete,
                        "Eliminar conexión…",
                        ContextAct::AskDelete,
                    ));
            }
            MenuTarget::Node(obj, kind) => match kind {
                ObjectKind::Database | ObjectKind::Schema => {
                    body = body
                        .push(menu_item(
                            icons::Icon::NewTable,
                            "Nueva tabla…",
                            ContextAct::NewTable,
                        ))
                        .push(menu_item(
                            icons::Icon::Refresh,
                            "Actualizar",
                            ContextAct::Refresh,
                        ))
                        .push(menu_item(
                            icons::Icon::Copy,
                            "Copiar nombre",
                            ContextAct::CopyName,
                        ));
                }
                ObjectKind::Table | ObjectKind::View => {
                    let engine = app.engine_of(obj.connection);
                    let sql_db = engine != Engine::Mongodb;
                    body = body.push(menu_item(
                        icons::Icon::Open,
                        "Abrir",
                        ContextAct::Open,
                    ));
                    if *kind == ObjectKind::Table && sql_db {
                        body = body.push(menu_item(
                            icons::Icon::Design,
                            "Diseñar",
                            ContextAct::Design,
                        ));
                    }
                    body = body
                        .push(menu_item(
                            icons::Icon::Refresh,
                            "Actualizar",
                            ContextAct::Refresh,
                        ))
                        .push(menu_item(
                            icons::Icon::Copy,
                            "Copiar nombre",
                            ContextAct::CopyName,
                        ));
                    if sql_db {
                        body = body.push(menu_item(
                            icons::Icon::Delete,
                            "Eliminar…",
                            ContextAct::AskDelete,
                        ));
                    }
                }
                ObjectKind::Collection => {
                    body = body
                        .push(menu_item(
                            icons::Icon::Open,
                            "Abrir",
                            ContextAct::Open,
                        ))
                        .push(menu_item(
                            icons::Icon::Refresh,
                            "Actualizar",
                            ContextAct::Refresh,
                        ))
                        .push(menu_item(
                            icons::Icon::Copy,
                            "Copiar nombre",
                            ContextAct::CopyName,
                        ));
                }
                _ => {
                    body = body.push(menu_item(
                        icons::Icon::Copy,
                        "Copiar nombre",
                        ContextAct::CopyName,
                    ));
                }
            },
        }
    }
    container(body)
        .padding(6)
        .width(Length::Fill)
        .style(theme::workspace)
        .into()
}

fn push_tree<'a>(
    app: &'a App,
    parent: &ObjectRef,
    depth: usize,
    mut col: Column<'a, Message>,
) -> Column<'a, Message> {
    let filter_active = !app.tree_filter.trim().is_empty();
    let Some(children) = app.tree.get(parent) else {
        return col;
    };
    for node in children {
        if filter_active
            && !node_matches(&node.label, &app.tree_filter)
            && !subtree_matches(app, &node.object, &app.tree_filter)
        {
            continue;
        }
        let is_open = app.expanded.contains(&node.object);
        let marker = match (node.has_children, is_open) {
            (true, true) => "▾",
            (true, false) => "▸",
            (false, _) => "·",
        };
        let press = match node.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::Collection => {
                Message::TreeOpenObject(node.object.clone(), node.kind)
            }
            ObjectKind::Database | ObjectKind::Schema => {
                Message::TreeToggle(node.object.clone(), node.kind)
            }
            _ => Message::TreeSelect(node.object.clone(), node.kind),
        };
        let is_selected = app
            .selected_object
            .as_ref()
            .is_some_and(|(obj, _)| *obj == node.object);
        let row_style: fn(&Theme, button::Status) -> button::Style = if is_selected {
            theme::row_selected
        } else {
            theme::flat
        };
        let label = row![
            text(marker).size(11).color(theme::muted()),
            icons::svg(icons::Icon::for_kind(node.kind), 14.0),
            text(node.label.clone()).size(13),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        col = col.push(
            MouseArea::new(
                button(
                    container(label)
                        .padding(iced::Padding {
                            top: 2.0,
                            right: 6.0,
                            bottom: 2.0,
                            left: 6.0 + depth as f32 * 14.0,
                        }),
                )
                .width(Length::Fill)
                .style(row_style)
                .on_press(press),
            )
            .on_right_press(Message::ContextOpen(MenuTarget::Node(
                node.object.clone(),
                node.kind,
            ))),
        );
        if let Some(menu) = &app.context_menu {
            if matches!(
                &menu.target,
                MenuTarget::Node(o, k) if *o == node.object && *k == node.kind
            ) {
                col = col.push(context_menu_view(app, menu));
            }
        }
        if is_open || filter_active {
            col = push_tree(app, &node.object, depth + 1, col);
        }
    }
    col
}

/// Thickness of the invisible OS-style resize borders.
const RESIZE_EDGE: f32 = 6.0;
/// Size of the invisible resize corners.
const RESIZE_CORNER: f32 = 12.0;

/// Invisible strip along a window edge that starts an OS resize drag.
/// `horizontal` = top/bottom edge; `start` = top/left side.
fn resize_edge_zone(
    direction: iced::window::Direction,
    horizontal: bool,
    start: bool,
) -> Element<'static, Message> {
    use iced::mouse::Interaction;

    let interaction = if horizontal {
        Interaction::ResizingVertically
    } else {
        Interaction::ResizingHorizontally
    };
    let align = if start {
        iced::Alignment::Start
    } else {
        iced::Alignment::End
    };
    // `Stack` lays every layer at the origin, so the full-size container
    // positions the thin MouseArea strip on the actual window edge.
    let space = if horizontal {
        Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(RESIZE_EDGE))
    } else {
        Space::new()
            .width(Length::Fixed(RESIZE_EDGE))
            .height(Length::Fill)
    };
    let area = MouseArea::new(space)
        .on_press(Message::WinResize(direction))
        .interaction(interaction);
    let zone = container(area)
        .width(Length::Fill)
        .height(Length::Fill);
    if horizontal {
        zone.align_y(align)
    } else {
        zone.align_x(align)
    }
    .into()
}

/// Invisible corner square that starts an OS diagonal resize drag.
fn resize_corner_zone(
    direction: iced::window::Direction,
    align_x: iced::Alignment,
    align_y: iced::Alignment,
) -> Element<'static, Message> {
    use iced::mouse::Interaction;

    let interaction = match direction {
        iced::window::Direction::NorthWest | iced::window::Direction::SouthEast => {
            Interaction::ResizingDiagonallyDown
        }
        _ => Interaction::ResizingDiagonallyUp,
    };
    let space = Space::new()
        .width(Length::Fixed(RESIZE_CORNER))
        .height(Length::Fixed(RESIZE_CORNER));
    container(
        MouseArea::new(space)
            .on_press(Message::WinResize(direction))
            .interaction(interaction),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(align_x)
    .align_y(align_y)
    .into()
}

/// Windows-only frameless fix: winit's undecorated windows keep the frame
/// styles (WS_CAPTION etc.) for aero-snap support while spreading the client
/// over the non-client area. On some machines DWM bakes a caption-sized
/// presentation offset from those styles at window creation, which shifts the
/// rendered surface down while input stays at the true client coordinates
/// (see rust-windowing/winit#2419, emilk/egui#2770). Stripping the styles is
/// not enough: DWM only re-lays the composition out after an actual size
/// change, hence the +1px/-1px nudge at startup.
#[cfg(target_os = "windows")]
mod sys {
    use core::ffi::c_void;

    #[repr(C)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        pub fn SetWindowLongW(hwnd: *mut c_void, index: i32, new: i32) -> i32;
        pub fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        pub fn SetWindowPos(
            hwnd: *mut c_void,
            after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }

    pub const GWL_STYLE: i32 = -16;
    /// WS_CAPTION | WS_SYSMENU | WS_SIZEBOX | WS_MINIMIZEBOX | WS_MAXIMIZEBOX.
    pub const FRAME_STYLES: i32 = 0x00CF_0000;
    pub const SWP_NOSIZE: u32 = 0x0001;
    pub const SWP_NOMOVE: u32 = 0x0002;
    pub const SWP_NOZORDER: u32 = 0x0004;
    pub const SWP_NOACTIVATE: u32 = 0x0010;
    pub const SWP_FRAMECHANGED: u32 = 0x0020;

    /// HWND of an iced window handle (Windows variant).
    pub fn hwnd_of(window: &dyn iced::window::Window) -> Option<*mut c_void> {
        use iced::window::raw_window_handle::RawWindowHandle;

        let handle = window.window_handle().ok()?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return None;
        };
        Some(win32.hwnd.get() as *mut c_void)
    }

    /// Drop the frame styles winit keeps for snap/shake and tell DWM the
    /// frame changed. Idempotent: only touches the window when any frame
    /// style bit is still set.
    pub fn strip(window: &dyn iced::window::Window) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            if style & FRAME_STYLES != 0 {
                SetWindowLongW(hwnd, GWL_STYLE, style & !FRAME_STYLES);
                SetWindowPos(
                    hwnd,
                    core::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }
    }

    /// Resize by `delta` pixels without moving: forces the WM_NCCALCSIZE /
    /// DWM recomposition that makes the presentation follow the client area.
    pub fn nudge(window: &dyn iced::window::Window, delta: i32) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        unsafe {
            let mut rect = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return;
            }
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            SetWindowPos(
                hwnd,
                core::ptr::null_mut(),
                rect.left,
                rect.top,
                width,
                height + delta,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Re-assert the frameless style on an existing window (no resize).
fn enforce_title_frame(id: iced::window::Id) -> Task<Message> {
    #[cfg(target_os = "windows")]
    {
        iced::window::run(id, sys::strip).map(|()| Message::WinFrameChecked)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        Task::none()
    }
}

/// Startup-only height nudge so DWM drops the baked caption offset.
fn nudge_title_frame(id: iced::window::Id, delta: i32) -> Task<Message> {
    #[cfg(target_os = "windows")]
    {
        iced::window::run(id, move |window| sys::nudge(window, delta))
            .map(|()| Message::WinFrameChecked)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (id, delta);
        Task::none()
    }
}

fn main() -> iced::Result {
    let window = iced::window::Settings {
        // Custom title bar: the toolbar row doubles as the window chrome.
        decorations: false,
        min_size: Some(iced::Size::new(900.0, 560.0)),
        ..Default::default()
    };
    // Keep `undecorated_shadow` off (it draws a 1px line) and
    // `corner_preference` default (DWM rounds frameless corners itself).
    // The DWM caption-offset workaround lives in `enforce_title_frame`.

    iced::application(boot, App::update, App::view)
        .window(window)
        .title("jorgenio dbstudio")
        .theme(App::theme)
        .subscription(App::subscription)
        .run()
}

/// Floating suggestion list anchored at the editor's bottom-left corner.
fn completion_popup(completion: &Completion) -> Element<'_, Message> {
    let rows = completion
        .items
        .iter()
        .enumerate()
        .fold(Column::new().spacing(0), |col, (i, word)| {
            col.push(
                button(text(word).size(13))
                    .on_press(Message::CompletionAccept(i))
                    .width(Length::Fill)
                    .style(if i == completion.selected {
                        theme::completion_selected
                    } else {
                        theme::completion_item
                    }),
            )
        });
    let panel = container(
        column![
            text(format!(
                "{} sugerencias · Tab/Enter acepta · Esc cierra",
                completion.items.len()
            ))
            .size(11)
            .color(theme::muted()),
            scrollable(rows).height(Length::Fixed(200.0)),
        ]
        .padding(8)
        .spacing(6)
        .max_width(320.0),
    )
    .style(theme::completion_popup)
    .padding(4);
    // `Stack` lays every layer at the origin; a full-size wrapper anchors
    // the panel at the editor's bottom-left corner.
    container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::Start)
        .align_y(iced::Alignment::End)
        .padding(iced::Padding { top: 0.0, right: 0.0, bottom: 8.0, left: 8.0 })
        .into()
}

/// Walk the object tree in the background and collect every table/view/
/// collection and column label into a completion dictionary. One query per
/// container (database/schema/table), so even a few hundred tables only cost
/// a few hundred roundtrips — done off the UI thread after connecting.
async fn load_completion_words(session: Arc<Session>, root: ObjectRef) -> Vec<String> {
    let mut words = Vec::new();
    let mut stack = vec![root];
    let mut visited = HashSet::new();
    while let Some(parent) = stack.pop() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        let Ok(children) = session.client.list_children(&parent).await else {
            continue;
        };
        for node in children {
            match node.kind {
                ObjectKind::Database | ObjectKind::Schema => {
                    stack.push(node.object);
                }
                ObjectKind::Table => {
                    words.push(node.label);
                    stack.push(node.object);
                }
                ObjectKind::Column | ObjectKind::View | ObjectKind::Collection => {
                    words.push(node.label);
                }
                _ => {}
            }
        }
    }
    words.sort();
    words.dedup();
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_tab_successor_math() {
        // Close before the active one → active shifts left.
        assert_eq!(active_after_close(3, 2, 0), 1);
        // Close after the active one → active stays.
        assert_eq!(active_after_close(3, 0, 2), 0);
        // Close a middle active tab → the next one slides into its slot.
        assert_eq!(active_after_close(3, 1, 1), 1);
        // Close the LAST active tab → focus steps back.
        assert_eq!(active_after_close(3, 2, 2), 1);
        // Two tabs: close the active first one → second becomes active(0).
        assert_eq!(active_after_close(2, 0, 0), 0);
        // Two tabs: close the active last one → first becomes active(0).
        assert_eq!(active_after_close(2, 1, 1), 0);
    }

    #[test]
    fn tab_new_starts_fresh_and_unbound() {
        let tab = Tab::new("Consulta 7".to_string(), None);
        assert_eq!(tab.title, "Consulta 7");
        assert_eq!(tab.content, "");
        assert_eq!(tab.connection, None);
        assert!(!tab.executed);
        assert_eq!(tab.last_row_count, 0);
        assert!(tab.last_columns.is_empty());
        assert_eq!(tab.query_context, None);
    }

    #[test]
    fn completion_filters_case_insensitively() {
        let mut c = Completion {
            dictionary: vec!["usuarios".into(), "user_roles".into(), "pedidos".into()],
            ..Default::default()
        };
        c.push_char('U');
        c.push_char('s');
        assert!(c.open);
        assert_eq!(c.items.len(), 2, "usuarios y user_roles empiezan por us");
        c.push_char('u');
        assert_eq!(c.items, vec!["usuarios".to_string()]);
        assert_eq!(c.selected_word(), Some("usuarios"));
    }

    #[test]
    fn completion_closes_on_no_match_and_reopens() {
        let mut c = Completion {
            dictionary: vec!["usuarios".into()],
            ..Default::default()
        };
        c.push_char('z');
        assert!(!c.open);
        c.pop_char();
        c.push_char('u');
        assert!(c.open);
    }

    #[test]
    fn completion_prefix_is_capped_and_navigable() {
        let mut c = Completion {
            dictionary: (0..60).map(|i| format!("tabla_{i:02}")).collect(),
            ..Default::default()
        };
        c.push_char('t');
        assert_eq!(c.items.len(), 50);
        c.move_down();
        assert_eq!(c.selected, 1);
        c.move_up();
        c.move_up();
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn completion_open_all_lists_up_to_fifty() {
        let mut c = Completion {
            dictionary: (0..80).map(|i| format!("w{i}")).collect(),
            ..Default::default()
        };
        c.open_all();
        assert!(c.open);
        assert_eq!(c.items.len(), 50);
    }

    #[test]
    fn completion_open_all_with_empty_dictionary_stays_closed() {
        let mut c = Completion::default();
        c.open_all();
        assert!(!c.open);
        assert!(c.items.is_empty());
    }

    fn obj(path: &[&str]) -> ObjectRef {
        ObjectRef {
            connection: ConnectionId::nil(),
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn pg_table_query_drops_database_segment() {
        let sql = query_for_engine(Engine::Postgres, &obj(&["appdb", "public", "usuarios"]), ObjectKind::Table);
        assert_eq!(sql, "SELECT * FROM \"public\".\"usuarios\" LIMIT 100;");
    }

    #[test]
    fn sqlite_table_query_drops_file_label() {
        let sql = query_for_engine(Engine::Sqlite, &obj(&["demo.db", "usuarios"]), ObjectKind::Table);
        assert_eq!(sql, "SELECT * FROM \"usuarios\" LIMIT 100;");
    }

    #[test]
    fn mysql_table_query_keeps_database() {
        let sql = query_for_engine(Engine::Mysql, &obj(&["tienda", "pedidos"]), ObjectKind::Table);
        assert_eq!(sql, "SELECT * FROM `tienda`.`pedidos` LIMIT 100;");
    }

    #[test]
    fn mssql_cross_database_uses_dot_dot() {
        let sql = query_for_engine(Engine::SqlServer, &obj(&["ventas", "orders"]), ObjectKind::Table);
        assert_eq!(sql, "SELECT TOP 100 * FROM [ventas]..[orders];");
    }

    #[test]
    fn mongo_collection_is_extended_json() {
        let sql = query_for_engine(
            Engine::Mongodb,
            &obj(&["appdb", "usuarios"]),
            ObjectKind::Collection,
        );
        assert_eq!(
            sql,
            "{\n  \"find\": \"usuarios\",\n  \"filter\": {},\n  \"limit\": 100\n}"
        );
    }

    #[test]
    fn view_generates_select_like_table() {
        let sql = query_for_engine(Engine::Postgres, &obj(&["appdb", "public", "v_activos"]), ObjectKind::View);
        assert_eq!(sql, "SELECT * FROM \"public\".\"v_activos\" LIMIT 100;");
    }

    #[test]
    fn tree_filter_matches_case_insensitively() {
        assert!(node_matches("Pedidos", ""));
        assert!(node_matches("Pedidos", "   "));
        assert!(node_matches("pedidos_2024", "PEDID"));
        assert!(node_matches("usuarios", "suari"));
        assert!(!node_matches("usuarios", "pedidos"));
    }

    #[test]
    fn drop_statement_qualifies_per_engine() {
        // SQLite is file-scoped: no qualification.
        let t = obj(&["appdb", "pedidos"]);
        assert_eq!(
            drop_statement(Engine::Sqlite, ObjectKind::Table, &t),
            Some("DROP TABLE \"pedidos\";".into())
        );
        // MySQL: `db`.`table`.
        assert_eq!(
            drop_statement(Engine::Mysql, ObjectKind::Table, &t),
            Some("DROP TABLE `appdb`.`pedidos`;".into())
        );
        // Postgres: schema-qualified (path = [db, schema, table]).
        let pg = obj(&["appdb", "public", "v_activos"]);
        assert_eq!(
            drop_statement(Engine::Postgres, ObjectKind::View, &pg),
            Some("DROP VIEW \"public\".\"v_activos\";".into())
        );
        // Mongo collections and columns are not droppable this way.
        assert_eq!(drop_statement(Engine::Mongodb, ObjectKind::Collection, &t), None);
        assert_eq!(drop_statement(Engine::Postgres, ObjectKind::Column, &t), None);
        // Quote doubles inside identifiers.
        let weird = obj(&["a\"b"]);
        assert_eq!(
            drop_statement(Engine::Sqlite, ObjectKind::Table, &weird),
            Some("DROP TABLE \"a\"\"b\";".into())
        );
    }

    #[test]
    fn design_parent_follows_the_selection() {
        let id = ConnectionId::nil();
        // Selected database/schema becomes the container itself.
        assert_eq!(
            design_parent(id, Some(&(obj(&["tienda"]), ObjectKind::Database))),
            obj(&["tienda"])
        );
        assert_eq!(
            design_parent(id, Some(&(obj(&["appdb", "public"]), ObjectKind::Schema))),
            obj(&["appdb", "public"])
        );
        // A table's container is its parent path.
        assert_eq!(
            design_parent(id, Some(&(obj(&["tienda", "pedidos"]), ObjectKind::Table))),
            obj(&["tienda"])
        );
        // Collections and foreign connections fall back to the root.
        assert_eq!(
            design_parent(id, Some(&(obj(&["appdb", "usuarios"]), ObjectKind::Collection))),
            ObjectRef::root(id)
        );
        assert_eq!(design_parent(id, None), ObjectRef::root(id));
    }

    #[test]
    fn designer_apply_skips_comment_only_statements() {
        let mut s = designer::DesignerState::new_table(
            ConnectionId::nil(),
            ObjectRef {
                connection: ConnectionId::nil(),
                path: vec!["demo.db".into()],
            },
        );
        s.name = "nueva".into();
        // An unnamed new table previews nothing; once named, the CREATE
        // passes the filter while `--` limitation notes would not.
        let run = |s: &designer::DesignerState| -> Vec<String> {
            designer::preview(Engine::Sqlite, s)
                .into_iter()
                .filter(|st| !st.trim_start().starts_with("--"))
                .collect()
        };
        assert!(run(&s).iter().all(|st| st.starts_with("CREATE TABLE")));
        assert_eq!(run(&s).len(), 1);
        s.name = "  ".into();
        assert!(run(&s).is_empty());
    }
}


