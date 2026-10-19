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
mod import;
mod store;
mod sync;
mod theme;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbstudio_core::{
    CellValue, ColumnMeta, ConnectionConfig, ConnectionId, Engine, ExecuteRequest, ObjectKind,
    ObjectNode, ObjectRef, QueryResult, TypeFamily,
};
use dbstudio_drivers::{self as drivers, Session};
use form::{ConnForm, FormMsg};
use import::ImportFormat;
use store::Connections;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, rule, scrollable, stack, text, text_input,
    Column, MouseArea, Space,
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
    /// Data-edit strip state (travels with the tab like the grid).
    data_edit: Option<DataEditState>,
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
            data_edit: None,
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
    /// Objects-panel overlay for the selected container, when open.
    objects: Option<ObjectsState>,
    /// Last left-click on a navigator row, for time-based double click.
    tree_last_click: Option<(TreeClickTarget, std::time::Instant)>,
    /// Data-grid edit overlay (single-table results with a primary key).
    data_edit: Option<DataEditState>,
    /// Object + SQL of the generated SELECT currently running; consumed
    /// when its rows land (`apply_result`) to open the edit strip.
    data_pending: Option<(ObjectRef, String)>,
    /// Import wizard overlay (CSV/TSV/JSON → batch INSERT), when open.
    import: Option<ImportState>,
    /// Auto-refresh interval for the result grid (timer via subscription).
    auto_refresh: RefreshEvery,
    /// Users/roles overlay (listing + DDL form), when open.
    users: Option<UsersState>,
    /// Executed statements, newest first (persisted to `history.json`).
    history: Vec<store::HistoryEntry>,
    /// Query-history overlay, when open.
    history_panel: Option<HistoryPanel>,
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
    /// Objects panel: show the children of a container (connection root,
    /// database or schema) as a Navicat-style list in the workspace.
    ObjectsOpen(ObjectRef, ObjectKind),
    /// Close the objects panel (✕ or opening a query tab from a row).
    ObjectsClose,
    /// Panel search box.
    ObjectsFilter(String),
    /// Row click; two quick clicks (by time) open the object as a tab.
    ObjectsRowClick(usize),
    /// Toolbar actions of the objects panel.
    ObjectsAction(ObjectsAction),
    // ── data grid editing (single-table results with a primary key) ────
    /// Draft a cell of the selected row: (row, col, text).
    DataField(usize, usize, String),
    /// Toggle the NULL flag of a cell of the selected row.
    DataFieldNull(usize, usize, bool),
    /// Draft a column of the pending insert row.
    DataInsertField(usize, String),
    /// Toggle the NULL flag of a column of the pending insert row.
    DataInsertFieldNull(usize, bool),
    /// Start composing a new row ("Insertar fila").
    DataStartInsert,
    /// Arm the two-step mark-for-deletion of the selected row.
    DataDeleteAsk,
    /// Mark the selected row for deletion (applied on "Aplicar").
    DataDeleteConfirm,
    /// Leave the deletion confirmation step.
    DataDeleteCancel,
    /// Apply all pending edits (UPDATE/INSERT/DELETE) and refresh.
    DataApply,
    /// Discard every pending change (drafts, insert, deletions).
    DataCancel,
    /// Close the data-edit strip entirely (grid stays read-only).
    DataClose,
    /// `edit_statements` batch finished; refresh the grid on success.
    DataApplied(ObjectRef, Result<usize, String>),
    /// `describe_table` answered for the data-edit strip (primary keys).
    DataDescribed(ObjectRef, Result<drivers::TableDetail, String>),
    /// Open the import wizard for a table (native file dialog included).
    ImportBrowse,
    /// Native open-file dialog returned a path (None = cancelled).
    ImportPicked(Option<std::path::PathBuf>),
    /// Path typed by hand in the wizard.
    ImportSetPath(String),
    ImportFormatSel(ImportFormat),
    ImportDelimiter(char),
    ImportHeader(bool),
    /// Re-read + re-parse the file with the current wizard settings.
    ImportReload,
    ImportLoaded(Result<import::Parsed, String>),
    ImportDescribed(ObjectRef, Result<drivers::TableDetail, String>),
    /// Mapping row changed: file column → target column (None = skip).
    ImportMap(usize, Option<usize>),
    ImportRun,
    ImportDone(Result<usize, String>),
    ImportClose,
    /// Auto-refresh interval picked in the result strip.
    AutoSet(RefreshEvery),
    /// Auto-refresh timer fired: re-run the active editor's read-only SQL.
    AutoTick,
    /// Respaldo SQL: el diálogo de guardado devolvió destino (o canceló).
    DumpTo(ObjectRef, Option<std::path::PathBuf>),
    /// Respaldo terminado: ruta escrita y filas volcadas, o error.
    DumpDone(Result<(String, usize), String>),
    // ── gestión de usuarios/roles de la conexión ──
    UsersClose,
    UsersRefresh,
    UsersLoaded(Result<Vec<UserRow>, String>),
    UsersCreate,
    UsersAlter(String),
    UsersDropAsk(String),
    UsersDropCancel,
    UsersDropConfirm(String),
    UsersEditCancel,
    UsersField(UserField, String),
    UsersToggleLogin,
    UsersToggleSuper,
    UsersRun,
    UsersApplied(Result<String, String>),
    /// Open the save dialog for the current editor content.
    EditorSave,
    /// Save dialog answered; `None` = cancelled.
    EditorSaveTo(Option<std::path::PathBuf>),
    /// Background write of the editor content finished.
    EditorSaved(Result<String, String>),
    /// Open the file picker for a `.sql` file.
    EditorOpen,
    /// File picker answered; `None` = cancelled.
    EditorOpenTo(Option<std::path::PathBuf>),
    /// Background read finished: display path + file content.
    EditorOpened(Result<(String, String), String>),
    /// Show the query-history overlay.
    HistoryOpen,
    HistoryClose,
    /// Substring filter typed in the history strip.
    HistoryFilter(String),
    /// Load entry (index into `App::history`) into the editor.
    HistoryPick(usize),
    /// Ask before wiping the whole history.
    HistoryClearAsk,
    HistoryClearNo,
    HistoryClearYes,
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
    /// Open the import wizard for a table (CSV/TSV/JSON → INSERT).
    ImportData,
    /// Open the save dialog for a SQL dump (DROP + CREATE + INSERT).
    DumpData,
    /// Open the users/roles overlay for a connection.
    Users,
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

/// Toolbar actions of the objects panel.
#[derive(Clone, Copy, Debug)]
enum ObjectsAction {
    /// Force a fresh listing of the container's children.
    Refresh,
    /// New table under the panel's database/schema.
    NewTable,
    /// Open the designer for the selected table row.
    Design,
    /// Open the import wizard for the selected table row.
    Import,
    /// Arm the two-step delete for the selected table/view row.
    Delete,
    /// Run the DROP armed by `Delete`.
    ConfirmDelete,
    /// Disarm the delete confirmation.
    CancelDelete,
}

/// Objects-panel overlay: the children of the selected container shown as
/// a Navicat-style list (toolbar + search + rows) in the central area.
struct ObjectsState {
    /// Container whose children are listed (connection root / db / schema).
    container: ObjectRef,
    /// Kind of `container` (Connection, Database or Schema).
    kind: ObjectKind,
    /// Panel search box text.
    filter: String,
    /// Selected row index (into the *filtered* rows).
    selected: Option<usize>,
    /// Last click per row for time-based double click detection.
    last_click: Option<(usize, std::time::Instant)>,
    /// Destructive delete confirmation showing.
    confirm: bool,
}

/// Import wizard overlay: reads a CSV/TSV/JSON file, maps its columns onto
/// an existing table and batch-inserts the rows (m17).
struct ImportState {
    /// Target table (SQL engines only).
    table: ObjectRef,
    /// Connection owning the table (session for describe + insert).
    connection: ConnectionId,
    /// Target columns from `describe_table`.
    columns: Vec<ColumnMeta>,
    /// `describe_table` still in flight.
    loading: bool,
    /// File path (picked with the dialog or typed by hand).
    path: String,
    format: ImportFormat,
    /// CSV delimiter (TSV forces tab, JSON ignores it).
    delimiter: char,
    /// First row holds column names (CSV/TSV only).
    header: bool,
    /// File's column names (header row or generated `colN`).
    file_columns: Vec<String>,
    /// Parsed raw rows.
    rows: Vec<Vec<String>>,
    /// Target column per file column (`None` = skip).
    mapping: Vec<Option<usize>>,
    /// Last parse error (red banner inside the wizard).
    error: Option<String>,
    /// Insert batch in flight.
    running: bool,
}

impl ImportState {
    fn new(table: ObjectRef, connection: ConnectionId) -> Self {
        Self {
            table,
            connection,
            columns: Vec::new(),
            loading: true,
            path: String::new(),
            format: ImportFormat::Csv,
            delimiter: ',',
            header: true,
            file_columns: Vec::new(),
            rows: Vec::new(),
            mapping: Vec::new(),
            error: None,
            running: false,
        }
    }

    /// Rebuild the mapping from names (after a file load or the describe).
    fn remap(&mut self) {
        self.mapping = import::auto_map(&self.file_columns, &self.columns);
    }

    fn mapped_count(&self) -> usize {
        self.mapping.iter().flatten().count()
    }

    /// First duplicate target column (pre-run validation).
    fn duplicate_target(&self) -> Option<String> {
        let mut seen: HashSet<usize> = HashSet::new();
        for target in self.mapping.iter().flatten() {
            if !seen.insert(*target) {
                return Some(format!(
                    "la columna «{}» está mapeada dos veces",
                    self.columns.get(*target)?.name
                ));
            }
        }
        None
    }
}

/// One option of a mapping pick-list: target column index + display label.
#[derive(Clone, PartialEq)]
struct MapChoice(Option<usize>, String);

impl std::fmt::Display for MapChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

/// Delimiter pick-list entry (a bare `char` would render tab invisibly).
#[derive(Clone, Copy, PartialEq)]
struct Delim(char);

impl std::fmt::Display for Delim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            ',' => ", (coma)",
            ';' => "; (punto y coma)",
            '\t' => "TAB (tabulador)",
            '|' => "| (barra)",
            other => return write!(f, "{other}"),
        })
    }
}

const DELIMS: [Delim; 4] = [Delim(','), Delim(';'), Delim('\t'), Delim('|')];

/// Auto-refresh interval of the result grid (`Off` = disabled).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RefreshEvery {
    Off,
    Sec1,
    Sec5,
    Sec10,
    Sec30,
}

impl RefreshEvery {
    const ALL: [RefreshEvery; 5] = [
        RefreshEvery::Off,
        RefreshEvery::Sec1,
        RefreshEvery::Sec5,
        RefreshEvery::Sec10,
        RefreshEvery::Sec30,
    ];

    fn period(self) -> Option<std::time::Duration> {
        use std::time::Duration;
        use RefreshEvery::*;
        match self {
            Off => None,
            Sec1 => Some(Duration::from_secs(1)),
            Sec5 => Some(Duration::from_secs(5)),
            Sec10 => Some(Duration::from_secs(10)),
            Sec30 => Some(Duration::from_secs(30)),
        }
    }
}

impl std::fmt::Display for RefreshEvery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RefreshEvery::Off => "no",
            RefreshEvery::Sec1 => "1 s",
            RefreshEvery::Sec5 => "5 s",
            RefreshEvery::Sec10 => "10 s",
            RefreshEvery::Sec30 => "30 s",
        })
    }
}

/// Guard for the auto-refresh timer: only a single read-only statement
/// re-runs unattended. Word-mangled uppercase scan rejects anything with a
/// data/DDL keyword anywhere (including data-modifying `WITH` CTEs) and any
/// multi-statement payload.
fn auto_refreshable(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim_end();
    if trimmed.is_empty() || trimmed.contains(';') {
        return false;
    }
    // Blank out everything that is not alphanumeric so word boundaries are
    // plain spaces (`--comment; DELETE…` still contains " DELETE ").
    let flat: String = trimmed
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let flat = format!(" {} ", flat.to_ascii_uppercase());
    for bad in [
        " INSERT ",
        " UPDATE ",
        " DELETE ",
        " DROP ",
        " ALTER ",
        " TRUNCATE ",
        " CREATE ",
        " REPLACE ",
        " MERGE ",
        " ATTACH ",
        " VACUUM ",
        " SET ",
        " GRANT ",
        " REVOKE ",
    ] {
        if flat.contains(bad) {
            return false;
        }
    }
    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "SELECT" | "WITH" | "SHOW" | "PRAGMA" | "EXPLAIN" | "DESCRIBE"
    )
}

/// Query-history overlay state: substring filter + delete confirmation.
#[derive(Clone, Debug)]
struct HistoryPanel {
    filter: String,
    clear_confirm: bool,
}

/// Insert `sql` at the head of the history (duplicates move up, blank
/// input is ignored) and trim the tail to [`store::HISTORY_CAP`].
fn history_push(entries: &mut Vec<store::HistoryEntry>, sql: String, conn: Option<String>) {
    if sql.trim().is_empty() {
        return;
    }
    let when = chrono::Local::now().format("%d/%m %H:%M").to_string();
    if let Some(pos) = entries.iter().position(|e| e.sql == sql) {
        let mut entry = entries.remove(pos);
        entry.conn = conn;
        entry.when = when;
        entries.insert(0, entry);
        return;
    }
    entries.insert(
        0,
        store::HistoryEntry {
            sql,
            conn,
            when,
        },
    );
    entries.truncate(store::HISTORY_CAP);
}

/// One-line preview for the history rows: whitespace collapsed, the tail
/// cut past 120 chars.
fn history_line(sql: &str) -> String {
    const MAX: usize = 120;
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX {
        flat.chars().take(MAX).collect::<String>() + "…"
    } else {
        flat
    }
}

/// Account listing row: `name` is the account, `host` exists only on
/// MySQL/MariaDB (identity is user@host), `detail` carries the second
/// reported column (host, role class, principal type…).
#[derive(Clone, Debug)]
struct UserRow {
    name: String,
    host: Option<String>,
    detail: String,
}

/// Editable field of the user form (drives `Message::UsersField`).
#[derive(Clone, Copy, Debug)]
enum UserField {
    Name,
    Password,
    Host,
}

/// Which statement family to generate for the form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UserAction {
    Create,
    Alter,
}

/// Create/alter form: `altered = Some(original name)` marks alter mode (v1
/// never renames accounts).
#[derive(Clone, Debug)]
struct UserEdit {
    name: String,
    password: String,
    host: String,
    can_login: bool,
    superuser: bool,
    altered: Option<String>,
}

impl UserEdit {
    fn create() -> Self {
        Self {
            name: String::new(),
            password: String::new(),
            host: "%".into(),
            can_login: true,
            superuser: false,
            altered: None,
        }
    }

    fn alter(row: &UserRow) -> Self {
        let detail = row.detail.to_ascii_lowercase();
        Self {
            name: row.name.clone(),
            password: String::new(),
            host: row.host.clone().unwrap_or_else(|| "%".into()),
            // Postgres CASE: "super" / "grupo" / "login". MySQL's detail is
            // the host (lock state unknown → assume active; the alter only
            // ever adds ACCOUNT LOCK/UNLOCK explicitly).
            can_login: !detail.contains("grupo"),
            superuser: detail.contains("super"),
            altered: Some(row.name.clone()),
        }
    }

    fn action(&self) -> UserAction {
        if self.altered.is_some() {
            UserAction::Alter
        } else {
            UserAction::Create
        }
    }
}

/// Users/roles overlay: read-only listing plus a create/alter form with an
/// inline DDL preview. Mutations v1 target MySQL/MariaDB and PostgreSQL;
/// SQL Server lists only; SQLite/MongoDB/Oracle refuse to open.
struct UsersState {
    connection: ConnectionId,
    engine: Engine,
    loading: bool,
    running: bool,
    users: Vec<UserRow>,
    error: Option<String>,
    edit: Option<UserEdit>,
    confirm_drop: Option<String>,
}

impl UsersState {
    fn new(connection: ConnectionId, engine: Engine) -> Self {
        Self {
            connection,
            engine,
            loading: true,
            running: false,
            users: Vec::new(),
            error: None,
            edit: None,
            confirm_drop: None,
        }
    }

    /// DDL is only generated for engines with tested statements (v1).
    fn can_mutate(&self) -> bool {
        matches!(
            self.engine,
            Engine::Mysql | Engine::MariaDb | Engine::Postgres
        )
    }
}

/// Which navigator row the last left-click hit (time-based double click;
/// `MouseArea::on_double_click` never fires wrapping a button because the
/// button captures the left press first).
#[derive(Clone, PartialEq, Debug)]
enum TreeClickTarget {
    Conn(ConnectionId),
    Node(ObjectRef),
}

/// Draft of one editable form field: the text as typed plus its NULL flag.
/// Compared against the original cell to detect dirty state.
#[derive(Clone, PartialEq, Debug)]
struct FieldDraft {
    text: String,
    null: bool,
}

impl FieldDraft {
    fn from_cell(cell: &CellValue) -> Self {
        match cell {
            CellValue::Null => Self {
                text: String::new(),
                null: true,
            },
            other => Self {
                text: other.as_display().into_owned(),
                null: false,
            },
        }
    }

    fn matches_cell(&self, cell: &CellValue) -> bool {
        &Self::from_cell(cell) == self
    }
}

/// Data-edit overlay for the result grid: active when the displayed rows
/// came from a generated single-table `SELECT` whose table has a primary
/// key (Navicat-style row editing in a strip above the grid).
struct DataEditState {
    /// Source table the SELECT was generated from.
    source: ObjectRef,
    /// Connection owning the table (session for apply/refresh).
    connection: ConnectionId,
    /// The generated SELECT; re-run after every successful apply.
    sql: String,
    /// Primary key column names (state is dropped when empty).
    pk: Vec<String>,
    /// `describe_table` still in flight (strip shows a waiting state).
    loading_pk: bool,
    /// Result columns: labels and type families for literals.
    columns: Vec<ColumnMeta>,
    /// Original rows as fetched (PK values and draft comparison).
    rows: Vec<Vec<CellValue>>,
    /// Row selected in the grid (index into `rows`).
    selected: Option<usize>,
    /// Pending new row: column → draft (shows the insert form instead).
    insert: Option<HashMap<usize, FieldDraft>>,
    /// Two-step mark-for-deletion confirmation showing.
    confirm_delete: bool,
    /// Pending cell edits: (row, col) → draft.
    drafts: HashMap<(usize, usize), FieldDraft>,
    /// Rows pending deletion (indexes into `rows`).
    deletes: HashSet<usize>,
}

impl DataEditState {
    /// Draft for a cell of the selected row, defaulting to the original.
    fn draft(&self, row: usize, col: usize) -> FieldDraft {
        self.drafts
            .get(&(row, col))
            .cloned()
            .or_else(|| {
                self.rows.get(row).and_then(|r| r.get(col)).map(FieldDraft::from_cell)
            })
            .unwrap_or(FieldDraft {
                text: String::new(),
                null: false,
            })
    }

    /// Draft for an insert-form column (blank until typed).
    fn insert_draft(&self, col: usize) -> FieldDraft {
        self.insert
            .as_ref()
            .and_then(|m| m.get(&col))
            .cloned()
            .unwrap_or(FieldDraft {
                text: String::new(),
                null: false,
            })
    }

    /// Pending operations: dirty cells + pending insert + marked deletions.
    fn pending_count(&self) -> usize {
        let dirty = self
            .drafts
            .iter()
            .filter(|((row, col), draft)| {
                self.rows
                    .get(*row)
                    .and_then(|r| r.get(*col))
                    .is_some_and(|orig| !draft.matches_cell(orig))
            })
            .count();
        dirty + usize::from(self.insert.is_some()) + self.deletes.len()
    }
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
            objects: None,
            tree_last_click: None,
            data_edit: None,
            data_pending: None,
            import: None,
            auto_refresh: RefreshEvery::Off,
            users: None,
            history: store::load_history(),
            history_panel: None,
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

/// Rows of the objects panel: the container's loaded children, narrowed by
/// the panel's filter box (index in this vec = row index in the panel).
fn filtered_nodes<'a>(
    tree: &'a HashMap<ObjectRef, Vec<ObjectNode>>,
    container: &ObjectRef,
    filter: &str,
) -> Vec<&'a ObjectNode> {
    tree.get(container)
        .map(|children| {
            children
                .iter()
                .filter(|n| node_matches(&n.label, filter))
                .collect()
        })
        .unwrap_or_default()
}

/// Time-based double click (iced reports one press per click; rows have
/// no native double-click event, and `MouseArea::on_double_click` cannot
/// see left presses when the content button captures them first).
fn is_double_click<K: PartialEq>(
    last: Option<&(K, std::time::Instant)>,
    key: &K,
    now: std::time::Instant,
) -> bool {
    matches!(
        last,
        Some((k, t)) if k == key && now.duration_since(*t) < std::time::Duration::from_millis(400)
    )
}

/// Spanish caption for an object kind (objects panel columns).
fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Connection => "Conexión",
        ObjectKind::Database => "Base de datos",
        ObjectKind::Schema => "Esquema",
        ObjectKind::Collection => "Colección",
        ObjectKind::Table => "Tabla",
        ObjectKind::View => "Vista",
        ObjectKind::Column => "Columna",
        ObjectKind::Index => "Índice",
        ObjectKind::Function => "Función",
        ObjectKind::Procedure => "Procedimiento",
        ObjectKind::Trigger => "Trigger",
        ObjectKind::User => "Usuario",
    }
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

/// Native open-file dialog for the import wizard: it blocks, so it runs on
/// its own thread and resumes through a oneshot channel (same pattern as
/// the export dialog).
fn import_dialog_task() -> Task<Message> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Datos", &["csv", "tsv", "txt", "json"])
            .add_filter("CSV", &["csv", "txt"])
            .add_filter("JSON", &["json"]);
        if let Some(docs) = dirs::document_dir() {
            dialog = dialog.set_directory(docs);
        }
        let _ = tx.send(dialog.pick_file());
    });
    Task::perform(
        async move { rx.await.unwrap_or(None) },
        Message::ImportPicked,
    )
}

/// Read + parse the chosen file off the UI thread with the wizard settings.
fn import_load_task(
    path: String,
    format: ImportFormat,
    delimiter: char,
    header: bool,
) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("no se pudo leer «{path}»: {e}"))?;
                import::parse(&text, format, delimiter, header)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::ImportLoaded,
    )
}

/// Describe the import target to learn its columns (mapping side).
fn import_describe_task(
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
            res
        },
        move |res| Message::ImportDescribed(reply.clone(), res),
    )
}

/// Run the import INSERT batches sequentially; `Ok(n)` is the number of rows
/// the batches carried (statement text is truncated in errors: multi-row
/// VALUES can be tens of KB).
fn import_apply_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    stmts: Vec<String>,
    expected_rows: usize,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                for stmt in &stmts {
                    client
                        .execute(ExecuteRequest::new(stmt))
                        .await
                        .map_err(|e| {
                            let shown: String = stmt.chars().take(240).collect();
                            let suffix = if stmt.chars().count() > 240 { "…" } else { "" };
                            format!("{shown}{suffix}\n  → {e}")
                        })?;
                }
                Ok::<usize, String>(expected_rows)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::ImportDone,
    )
}

/// Describe the data-edit strip's table to learn its primary keys.
fn data_describe_task(
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
            res
        },
        move |res| Message::DataDescribed(reply.clone(), res),
    )
}

/// Run the data-edit DML batch sequentially (same semantics as
/// `design_apply_task`); `DataApplied` carries the source table so a
/// late reply for a stale strip can be ignored.
fn data_apply_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    stmts: Vec<String>,
    source: ObjectRef,
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
        move |res| Message::DataApplied(source.clone(), res),
    )
}

/// Describe the dump target, read **every** row and write the SQL script to
/// `path`. `Ok((ruta, filas))`; the read is bounded only by memory — the
/// drivers cap on their streaming loop (`max_rows`), not in the SQL.
fn dump_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    engine: Engine,
    table: ObjectRef,
    path: std::path::PathBuf,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                let detail = client
                    .describe_table(&table)
                    .await
                    .map_err(|e| format!("no se pudo describir la tabla: {e}"))?;
                let select = format!("SELECT * FROM {}", table_ref(engine, &table));
                let result = client
                    .execute(ExecuteRequest::new(select).with_max_rows(usize::MAX))
                    .await
                    .map_err(|e| format!("no se pudieron leer las filas: {e}"))?;
                let text = dump_text(engine, &table, &detail, &result.rows)?;
                let display = path.display().to_string();
                let rows = result.rows.len();
                let shown = display.clone();
                tokio::task::spawn_blocking(move || {
                    std::fs::write(&path, text)
                        .map_err(|e| format!("no se pudo escribir «{shown}»: {e}"))
                })
                .await
                .map_err(|e| e.to_string())??;
                Ok::<(String, usize), String>((display, rows))
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::DumpDone,
    )
}

/// Load the account list of `engine` (read-only listing SQL).
fn users_list_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    engine: Engine,
) -> Task<Message> {
    let Some(sql) = users_list_sql(engine) else {
        return Task::none();
    };
    Task::perform(
        async move {
            rt.spawn(async move {
                let result = client
                    .execute(ExecuteRequest::new(sql))
                    .await
                    .map_err(|e| format!("no se pudieron leer los usuarios: {e}"))?;
                Ok::<Vec<UserRow>, String>(users_from_rows(engine, &result.rows))
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::UsersLoaded,
    )
}

/// Execute one user-management statement; `Ok` carries the status text.
fn users_exec_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: Arc<dyn drivers::DatabaseClient>,
    sql: String,
    done: String,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move {
                client
                    .execute(ExecuteRequest::new(&sql))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<String, String>(done)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        Message::UsersApplied,
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
            Message::Grid(msg) => {
                // A row click arms the data-edit form: the grid owns the
                // selection (identity-keyed), so read it after forwarding.
                let pressed = matches!(&msg, grid::GridEvent::RowPressed { .. });
                let task = self.grid.update(msg).map(Message::Grid);
                if pressed && self.data_edit.is_some() {
                    let sel = self.grid.selection().iter().next().copied();
                    if let Some(edit) = &mut self.data_edit {
                        edit.selected = sel.map(|id| id as usize);
                        edit.insert = None;
                        edit.confirm_delete = false;
                    }
                }
                task
            }
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
                let now = std::time::Instant::now();
                let dbl = is_double_click(
                    self.tree_last_click.as_ref(),
                    &TreeClickTarget::Conn(id),
                    now,
                );
                self.tree_last_click = Some((TreeClickTarget::Conn(id), now));
                self.context_menu = None;
                let Some(i) = self.entries.iter().position(|e| e.cfg.id == id) else {
                    return Task::none();
                };
                let root = ObjectRef::root(id);
                let toggle = if self.active_id == Some(id) && self.expanded.contains(&root) {
                    self.expanded.remove(&root);
                    self.selected = Some(i);
                    Task::none()
                } else {
                    self.selected = Some(i);
                    self.expanded.insert(root.clone());
                    self.select_connection(i)
                };
                if dbl {
                    // Second quick click on the same connection: list its
                    // databases in the objects panel (Navicat double-click).
                    self.expanded.insert(root.clone());
                    Task::batch([
                        toggle,
                        self.update(Message::ObjectsOpen(root, ObjectKind::Connection)),
                    ])
                } else {
                    toggle
                }
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
                    ContextAct::Users => {
                        let MenuTarget::Conn(id) = menu.target else {
                            return Task::none();
                        };
                        self.users_open(id)
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
                    ContextAct::ImportData => {
                        let MenuTarget::Node(obj, kind) = menu.target else {
                            return Task::none();
                        };
                        if kind != ObjectKind::Table {
                            return Task::none();
                        }
                        self.import_start(obj)
                    }
                    ContextAct::DumpData => {
                        let MenuTarget::Node(obj, kind) = menu.target else {
                            return Task::none();
                        };
                        if kind != ObjectKind::Table {
                            return Task::none();
                        }
                        if self.engine_of(obj.connection) == Engine::Mongodb {
                            self.status =
                                "MongoDB no usa SQL: el respaldo aplica solo a motores SQL"
                                    .to_string();
                            return Task::none();
                        }
                        let file = format!(
                            "{}.sql",
                            obj.path.last().map(String::as_str).unwrap_or("respaldo")
                        );
                        let (tx, rx) =
                            tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
                        std::thread::spawn(move || {
                            let mut dialog = rfd::FileDialog::new()
                                .set_file_name(file.as_str())
                                .add_filter("SQL", &["sql"]);
                            if let Some(docs) = dirs::document_dir() {
                                dialog = dialog.set_directory(docs);
                            }
                            let _ = tx.send(dialog.save_file());
                        });
                        Task::perform(
                            async move {
                                let path = rx.await.unwrap_or(None);
                                (obj, path)
                            },
                            |(table, path)| Message::DumpTo(table, path),
                        )
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
            Message::ObjectsOpen(node, kind) => {
                self.context_menu = None;
                self.objects = Some(ObjectsState {
                    container: node.clone(),
                    kind,
                    filter: String::new(),
                    selected: None,
                    last_click: None,
                    confirm: false,
                });
                // The panel reads `tree` live: make sure the listing is on
                // its way (expand keeps the node open in the sidebar too).
                self.expanded.insert(node.clone());
                if self.tree.contains_key(&node) {
                    Task::none()
                } else {
                    self.load_children(&node)
                }
            }
            Message::ObjectsClose => {
                self.objects = None;
                Task::none()
            }
            Message::ObjectsFilter(f) => {
                if let Some(o) = &mut self.objects {
                    o.filter = f;
                    o.selected = None;
                    o.last_click = None;
                    o.confirm = false;
                }
                Task::none()
            }
            Message::ObjectsRowClick(i) => {
                let now = std::time::Instant::now();
                let Some(o) = &mut self.objects else {
                    return Task::none();
                };
                if !is_double_click(o.last_click.as_ref(), &i, now) {
                    o.last_click = Some((i, now));
                    o.selected = Some(i);
                    o.confirm = false;
                    return Task::none();
                }
                o.last_click = None;
                let Some(row) =
                    filtered_nodes(&self.tree, &o.container, &o.filter)
                        .get(i)
                        .map(|n| (*n).clone())
                else {
                    return Task::none();
                };
                match row.kind {
                    // Leaves open as a query tab (Navicat double-click).
                    ObjectKind::Table | ObjectKind::View | ObjectKind::Collection => {
                        self.objects = None;
                        self.update(Message::TreeOpenObject(row.object, row.kind))
                    }
                    // Containers drill the panel deeper.
                    ObjectKind::Connection
                    | ObjectKind::Database
                    | ObjectKind::Schema => {
                        let node = row.object;
                        let kind = row.kind;
                        self.objects = Some(ObjectsState {
                            container: node.clone(),
                            kind,
                            filter: String::new(),
                            selected: None,
                            last_click: None,
                            confirm: false,
                        });
                        self.expanded.insert(node.clone());
                        if self.tree.contains_key(&node) {
                            Task::none()
                        } else {
                            self.load_children(&node)
                        }
                    }
                    _ => Task::none(),
                }
            }
            Message::ObjectsAction(act) => {
                let Some(o) = &self.objects else {
                    return Task::none();
                };
                let container = o.container.clone();
                let ckind = o.kind;
                let sel_row = {
                    let rows = filtered_nodes(&self.tree, &container, &o.filter);
                    o.selected
                        .and_then(|i| rows.get(i).map(|n| (*n).clone()))
                };
                match act {
                    ObjectsAction::Refresh => {
                        self.objects.as_mut().map(|o| o.confirm = false);
                        self.reload_children(&container)
                    }
                    ObjectsAction::NewTable => {
                        if !matches!(
                            ckind,
                            ObjectKind::Database | ObjectKind::Schema
                        ) {
                            return Task::none();
                        }
                        // DesignNew binds to the active connection: rebind
                        // the workspace when the panel belongs to another.
                        let bind = if self.active_id != Some(container.connection) {
                            match self
                                .entries
                                .iter()
                                .position(|e| e.cfg.id == container.connection)
                            {
                                Some(i) => self.select_connection(i),
                                None => {
                                    self.status =
                                        "conexión no disponible".to_string();
                                    return Task::none();
                                }
                            }
                        } else {
                            Task::none()
                        };
                        self.selected_object = Some((container, ckind));
                        let open = self.update(Message::DesignNew);
                        Task::batch([bind, open])
                    }
                    ObjectsAction::Design => {
                        let Some(row) = sel_row else {
                            return Task::none();
                        };
                        if row.kind != ObjectKind::Table {
                            return Task::none();
                        }
                        self.selected_object =
                            Some((row.object, ObjectKind::Table));
                        self.update(Message::DesignSelected)
                    }
                    ObjectsAction::Import => {
                        let Some(row) = sel_row else {
                            return Task::none();
                        };
                        if row.kind != ObjectKind::Table {
                            return Task::none();
                        }
                        self.import_start(row.object)
                    }
                    ObjectsAction::Delete => {
                        let Some(row) = sel_row else {
                            return Task::none();
                        };
                        if !matches!(
                            row.kind,
                            ObjectKind::Table | ObjectKind::View
                        ) || self.engine_of(row.object.connection) == Engine::Mongodb
                        {
                            return Task::none();
                        }
                        if let Some(o) = &mut self.objects {
                            o.confirm = true;
                        }
                        Task::none()
                    }
                    ObjectsAction::ConfirmDelete => {
                        let Some(row) = sel_row else {
                            return Task::none();
                        };
                        if !matches!(
                            row.kind,
                            ObjectKind::Table | ObjectKind::View
                        ) {
                            return Task::none();
                        }
                        let engine = self.engine_of(row.object.connection);
                        if engine == Engine::Mongodb {
                            return Task::none();
                        }
                        let Some(stmt) = drop_statement(engine, row.kind, &row.object)
                        else {
                            return Task::none();
                        };
                        let Some(session) =
                            self.sessions.get(&row.object.connection).cloned()
                        else {
                            self.status =
                                "sin sesión: no se puede eliminar".to_string();
                            return Task::none();
                        };
                        if let Some(o) = &mut self.objects {
                            o.confirm = false;
                        }
                        self.status =
                            format!("eliminando {}…", row.label);
                        drop_task(
                            self.rt.clone(),
                            session.client.clone(),
                            stmt,
                            row.object,
                        )
                    }
                    ObjectsAction::CancelDelete => {
                        if let Some(o) = &mut self.objects {
                            o.confirm = false;
                        }
                        Task::none()
                    }
                }
            }
            // ── data grid editing: drafts, insert, delete, apply ─────────
            Message::DataField(row, col, text) => {
                if let Some(edit) = &mut self.data_edit {
                    if edit.insert.is_none() && edit.selected == Some(row) {
                        let draft = FieldDraft { text, null: false };
                        match edit.rows.get(row).and_then(|r| r.get(col)) {
                            Some(orig) if draft.matches_cell(orig) => {
                                // Typed back to the original: not dirty.
                                edit.drafts.remove(&(row, col));
                            }
                            Some(_) => {
                                edit.drafts.insert((row, col), draft);
                            }
                            None => {}
                        }
                    }
                }
                Task::none()
            }
            Message::DataFieldNull(row, col, null) => {
                if let Some(edit) = &mut self.data_edit {
                    if edit.insert.is_none() && edit.selected == Some(row) {
                        let mut draft = edit.draft(row, col);
                        draft.null = null;
                        match edit.rows.get(row).and_then(|r| r.get(col)) {
                            Some(orig) if draft.matches_cell(orig) => {
                                edit.drafts.remove(&(row, col));
                            }
                            Some(_) => {
                                edit.drafts.insert((row, col), draft);
                            }
                            None => {}
                        }
                    }
                }
                Task::none()
            }
            Message::DataInsertField(col, text) => {
                if let Some(edit) = &mut self.data_edit {
                    if edit.insert.is_some() {
                        edit.insert
                            .get_or_insert_with(HashMap::new)
                            .insert(col, FieldDraft { text, null: false });
                    }
                }
                Task::none()
            }
            Message::DataInsertFieldNull(col, null) => {
                if let Some(edit) = &mut self.data_edit {
                    if let Some(map) = &mut edit.insert {
                        let draft = map
                            .entry(col)
                            .or_insert(FieldDraft {
                                text: String::new(),
                                null: false,
                            });
                        draft.null = null;
                    }
                }
                Task::none()
            }
            Message::DataStartInsert => {
                if let Some(edit) = &mut self.data_edit {
                    if !edit.loading_pk {
                        edit.insert.get_or_insert_with(HashMap::new);
                        edit.confirm_delete = false;
                    }
                }
                Task::none()
            }
            Message::DataDeleteAsk => {
                if let Some(edit) = &mut self.data_edit {
                    if !edit.loading_pk && edit.insert.is_none() && edit.selected.is_some() {
                        edit.confirm_delete = true;
                    }
                }
                Task::none()
            }
            Message::DataDeleteConfirm => {
                if let Some(edit) = &mut self.data_edit {
                    if let Some(row) = edit.selected.take() {
                        edit.deletes.insert(row);
                        // The row is going away: drop its drafts too.
                        edit.drafts.retain(|(r, _), _| *r != row);
                    }
                    edit.confirm_delete = false;
                }
                Task::none()
            }
            Message::DataDeleteCancel => {
                if let Some(edit) = &mut self.data_edit {
                    edit.confirm_delete = false;
                }
                Task::none()
            }
            Message::DataApply => {
                let Some(edit) = &self.data_edit else {
                    return Task::none();
                };
                if edit.loading_pk {
                    return Task::none();
                }
                let engine = self.engine_of(edit.connection);
                let stmts = edit_statements(engine, edit);
                if stmts.is_empty() {
                    self.status = "sin cambios pendientes".to_string();
                    return Task::none();
                }
                let source = edit.source.clone();
                let Some(session) = self.sessions.get(&edit.connection) else {
                    self.status = "sin sesión: no se puede aplicar".to_string();
                    return Task::none();
                };
                let client = session.client.clone();
                self.status = format!("aplicando {} sentencia(s)…", stmts.len());
                data_apply_task(self.rt.clone(), client, stmts, source)
            }
            Message::DataApplied(source, res) => match res {
                Ok(applied) => {
                    let refresh = self
                        .data_edit
                        .as_ref()
                        .filter(|e| e.source == source)
                        .map(|e| e.sql.clone());
                    match refresh {
                        Some(sql) => {
                            self.status =
                                format!("✓ {applied} sentencia(s) aplicadas — refrescando…");
                            self.data_pending = Some((source, sql.clone()));
                            match self
                                .data_edit
                                .as_ref()
                                .and_then(|e| self.sessions.get(&e.connection))
                            {
                                Some(session) => {
                                    let client = session.client.clone();
                                    let req =
                                        ExecuteRequest::new(sql).with_max_rows(MAX_ROWS);
                                    execute_task(self.rt.clone(), client, req, self.active_tab)
                                }
                                None => {
                                    self.data_edit = None;
                                    Task::none()
                                }
                            }
                        }
                        None => {
                            self.status = format!("✓ {applied} sentencia(s) aplicadas");
                            Task::none()
                        }
                    }
                }
                Err(e) => {
                    self.status = format!("error aplicando: {e}");
                    Task::none()
                }
            },
            Message::DataDescribed(table, res) => {
                let owns = self.data_edit.as_ref().is_some_and(|e| e.source == table);
                if !owns {
                    return Task::none();
                }
                match res {
                    Ok(detail) => {
                        let binary_pk = detail.primary_keys.iter().any(|name| {
                            detail
                                .columns
                                .iter()
                                .find(|c| c.name.eq_ignore_ascii_case(name))
                                .is_some_and(|c| c.data_type.family == TypeFamily::Bytes)
                        });
                        if detail.primary_keys.is_empty() {
                            self.data_edit = None;
                            self.status =
                                "tabla sin clave primaria: edición no disponible".to_string();
                        } else if binary_pk {
                            self.data_edit = None;
                            self.status =
                                "clave primaria binaria: edición no disponible".to_string();
                        } else if let Some(edit) = &mut self.data_edit {
                            edit.pk = detail.primary_keys;
                            edit.loading_pk = false;
                            self.status =
                                "edición de datos activa: selecciona una fila".to_string();
                        }
                    }
                    Err(e) => {
                        self.data_edit = None;
                        self.status = format!("no se pudo leer la clave primaria: {e}");
                    }
                }
                Task::none()
            }
            Message::DataCancel => {
                if let Some(edit) = &mut self.data_edit {
                    edit.drafts.clear();
                    edit.insert = None;
                    edit.deletes.clear();
                    edit.confirm_delete = false;
                }
                self.status = "cambios descartados".to_string();
                Task::none()
            }
            Message::DataClose => {
                self.data_edit = None;
                Task::none()
            }
            // ── import wizard: file, mapping, batch INSERT ───────────────
            Message::ImportBrowse => import_dialog_task(),
            Message::ImportPicked(path) => match path {
                Some(path) => {
                    let Some(st) = &mut self.import else {
                        return Task::none();
                    };
                    st.path = path.display().to_string();
                    st.format = ImportFormat::from_path(&path);
                    st.delimiter = match st.format {
                        ImportFormat::Tsv => '\t',
                        ImportFormat::Csv => import::sniff_file(&path).unwrap_or(','),
                        ImportFormat::Json => ',',
                    };
                    st.error = None;
                    let (path, format, delimiter, header) =
                        (st.path.clone(), st.format, st.delimiter, st.header);
                    import_load_task(path, format, delimiter, header)
                }
                None => {
                    // A fresh dialog with no typed path aborts the wizard;
                    // a hand-typed path survives (the dialog was optional).
                    if self.import.as_ref().is_some_and(|s| s.path.is_empty()) {
                        self.import = None;
                        self.status = "importación cancelada".to_string();
                    }
                    Task::none()
                }
            },
            Message::ImportSetPath(v) => {
                if let Some(st) = &mut self.import {
                    st.path = v;
                }
                Task::none()
            }
            Message::ImportFormatSel(f) => {
                if let Some(st) = &mut self.import {
                    st.format = f;
                    if f == ImportFormat::Tsv {
                        st.delimiter = '\t';
                    }
                }
                self.import_reload()
            }
            Message::ImportDelimiter(d) => {
                if let Some(st) = &mut self.import {
                    st.delimiter = d;
                }
                self.import_reload()
            }
            Message::ImportHeader(h) => {
                if let Some(st) = &mut self.import {
                    st.header = h;
                }
                self.import_reload()
            }
            Message::ImportReload => self.import_reload(),
            Message::ImportLoaded(res) => {
                let Some(st) = &mut self.import else {
                    return Task::none();
                };
                match res {
                    Ok(parsed) => {
                        st.file_columns = parsed.columns;
                        st.rows = parsed.rows;
                        st.error = None;
                        st.remap();
                        self.status = format!(
                            "{} filas × {} columnas listas",
                            st.rows.len(),
                            st.file_columns.len()
                        );
                    }
                    Err(e) => {
                        st.file_columns.clear();
                        st.rows.clear();
                        st.mapping.clear();
                        st.error = Some(e.clone());
                        self.status = format!("error leyendo el fichero: {e}");
                    }
                }
                Task::none()
            }
            Message::ImportDescribed(table, res) => {
                let owns = self.import.as_ref().is_some_and(|s| s.table == table);
                if !owns {
                    return Task::none();
                }
                match res {
                    Ok(detail) => {
                        if let Some(st) = &mut self.import {
                            st.columns = detail.columns;
                            st.loading = false;
                            // File arrived before the columns: build the
                            // name-based mapping now (nothing user-set yet).
                            if !st.file_columns.is_empty()
                                && st.mapping.iter().all(Option::is_none)
                            {
                                st.remap();
                            }
                        }
                        Task::none()
                    }
                    Err(e) => {
                        self.import = None;
                        self.status = format!("no se pudo leer la tabla: {e}");
                        Task::none()
                    }
                }
            }
            Message::ImportMap(file_idx, target) => {
                if let Some(st) = &mut self.import {
                    if let Some(slot) = st.mapping.get_mut(file_idx) {
                        if target.is_none_or(|t| t < st.columns.len()) {
                            *slot = target;
                        }
                    }
                }
                Task::none()
            }
            Message::ImportRun => {
                let Some(st) = &self.import else {
                    return Task::none();
                };
                if st.loading || st.running || st.error.is_some() {
                    return Task::none();
                }
                if st.rows.is_empty() {
                    self.status = "no hay filas para importar".to_string();
                    return Task::none();
                }
                if st.mapped_count() == 0 {
                    self.status = "mapea al menos una columna del fichero".to_string();
                    return Task::none();
                }
                if let Some(dup) = st.duplicate_target() {
                    self.status = dup;
                    return Task::none();
                }
                let engine = self.engine_of(st.connection);
                let table = st.table.clone();
                let connection = st.connection;
                let expected = st.rows.len();
                let stmts = match import::insert_statements(
                    engine,
                    &st.table,
                    &st.columns,
                    &st.mapping,
                    &st.rows,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        self.status = format!("error preparando el import: {e}");
                        return Task::none();
                    }
                };
                if stmts.is_empty() {
                    self.status = "nada que importar".to_string();
                    return Task::none();
                }
                let Some(session) = self.sessions.get(&connection) else {
                    self.status = "sin sesión: no se puede importar".to_string();
                    return Task::none();
                };
                let client = session.client.clone();
                if let Some(st) = &mut self.import {
                    st.running = true;
                }
                let name = table.path.last().cloned().unwrap_or_default();
                self.status = format!("importando {expected} filas en {name}…");
                import_apply_task(self.rt.clone(), client, stmts, expected)
            }
            Message::ImportDone(res) => {
                let Some(st) = self.import.as_ref() else {
                    return Task::none();
                };
                let table = st.table.clone();
                match res {
                    Ok(n) => {
                        self.import = None;
                        let name = table.path.last().cloned().unwrap_or_default();
                        self.status = format!("✓ {n} filas importadas en {name}");
                        // Refresh the edit strip when it shows this table.
                        let refresh = self
                            .data_edit
                            .as_ref()
                            .filter(|e| e.source == table)
                            .map(|e| e.sql.clone());
                        match refresh {
                            Some(sql) => {
                                self.data_pending = Some((table.clone(), sql.clone()));
                                match self.sessions.get(&table.connection) {
                                    Some(session) => {
                                        let client = session.client.clone();
                                        let req =
                                            ExecuteRequest::new(sql).with_max_rows(MAX_ROWS);
                                        execute_task(
                                            self.rt.clone(),
                                            client,
                                            req,
                                            self.active_tab,
                                        )
                                    }
                                    None => {
                                        self.data_edit = None;
                                        Task::none()
                                    }
                                }
                            }
                            None => Task::none(),
                        }
                    }
                    Err(e) => {
                        if let Some(st) = &mut self.import {
                            st.running = false;
                        }
                        self.status = format!("error importando: {e}");
                        Task::none()
                    }
                }
            }
            Message::ImportClose => {
                self.import = None;
                Task::none()
            }
            Message::AutoSet(interval) => {
                self.auto_refresh = interval;
                self.status = match interval.period() {
                    Some(p) => format!(
                        "auto-refresh cada {} s",
                        p.as_secs()
                    ),
                    None => "auto-refresh: no".to_string(),
                };
                Task::none()
            }
            Message::AutoTick => {
                // A modal/overlay owns the center or the result is being
                // edited: never re-run behind the user's back (drafts and
                // selected rows would go stale against fresh grid rows).
                if self.data_edit.is_some()
                    || self.import.is_some()
                    || self.sync.is_some()
                    || self.designer.is_some()
                    || self.er.is_some()
                    || self.objects.is_some()
                    || self.form.is_some()
                {
                    return Task::none();
                }
                let Some(id) = self.active_id else {
                    return Task::none();
                };
                let Some(session) = self.sessions.get(&id) else {
                    return Task::none();
                };
                let sql = self.editor.content();
                if !auto_refreshable(&sql) {
                    // Non read-only (or empty/edited): pause silently until
                    // the user writes a runnable statement again.
                    return Task::none();
                }
                let client = session.client.clone();
                let mut req = ExecuteRequest::new(sql).with_max_rows(MAX_ROWS);
                req.context = self.query_context.clone();
                // data_pending is never armed here: the auto path must not
                // (re)open the data-edit strip behind the user.
                execute_task(self.rt.clone(), client, req, self.active_tab)
            }
            Message::DumpTo(table, Some(path)) => {
                let engine = self.engine_of(table.connection);
                if engine == Engine::Mongodb {
                    self.status =
                        "MongoDB no usa SQL: el respaldo aplica solo a motores SQL"
                            .to_string();
                    return Task::none();
                }
                let Some(session) = self.sessions.get(&table.connection) else {
                    self.status =
                        "la conexión no está activa — selecciónala de nuevo".to_string();
                    return Task::none();
                };
                self.status = "respaldando…".to_string();
                dump_task(
                    self.rt.clone(),
                    session.client.clone(),
                    engine,
                    table,
                    path,
                )
            }
            Message::DumpTo(_, None) => {
                self.status = "respaldo cancelado".to_string();
                Task::none()
            }
            Message::DumpDone(Ok((ruta, n))) => {
                self.status = format!("✓ respaldo escrito: {ruta} ({n} filas)");
                Task::none()
            }
            Message::DumpDone(Err(e)) => {
                self.status = format!("respaldo falló: {e}");
                Task::none()
            }
            Message::UsersClose => {
                self.users = None;
                Task::none()
            }
            Message::UsersRefresh => {
                let Some(st) = &self.users else {
                    return Task::none();
                };
                let (engine, id) = (st.engine, st.connection);
                let Some(session) = self.sessions.get(&id).cloned() else {
                    self.status = "la conexión no está activa".to_string();
                    return Task::none();
                };
                if let Some(st) = &mut self.users {
                    st.loading = true;
                    st.error = None;
                    st.confirm_drop = None;
                }
                self.status = "leyendo usuarios…".to_string();
                users_list_task(self.rt.clone(), session.client.clone(), engine)
            }
            Message::UsersLoaded(res) => {
                let Some(st) = &mut self.users else {
                    return Task::none();
                };
                st.loading = false;
                match res {
                    Ok(users) => {
                        st.error = None;
                        let n = users.len();
                        st.users = users;
                        self.status = format!("{n} cuentas");
                    }
                    Err(e) => {
                        st.error = Some(e.clone());
                        self.status = format!("no se pudieron leer los usuarios: {e}");
                    }
                }
                Task::none()
            }
            Message::UsersCreate => {
                if let Some(st) = &mut self.users {
                    st.edit = Some(UserEdit::create());
                    st.error = None;
                    st.confirm_drop = None;
                }
                Task::none()
            }
            Message::UsersAlter(name) => {
                let row = self.users.as_ref().and_then(|st| {
                    st.users.iter().find(|r| r.name == name).cloned()
                });
                if let (Some(st), Some(row)) = (self.users.as_mut(), row) {
                    st.edit = Some(UserEdit::alter(&row));
                    st.error = None;
                    st.confirm_drop = None;
                }
                Task::none()
            }
            Message::UsersDropAsk(name) => {
                if let Some(st) = &mut self.users {
                    st.confirm_drop = Some(name);
                }
                Task::none()
            }
            Message::UsersDropCancel => {
                if let Some(st) = &mut self.users {
                    st.confirm_drop = None;
                    st.edit = None;
                    st.error = None;
                }
                Task::none()
            }
            Message::UsersEditCancel => {
                if let Some(st) = &mut self.users {
                    st.edit = None;
                    st.error = None;
                }
                Task::none()
            }
            Message::UsersDropConfirm(name) => {
                let Some(st) = &self.users else {
                    return Task::none();
                };
                let Some(row) = st.users.iter().find(|r| r.name == name).cloned() else {
                    return Task::none();
                };
                let sql = match user_drop_sql(st.engine, &row) {
                    Ok(s) => s,
                    Err(e) => {
                        self.status = format!("error: {e}");
                        return Task::none();
                    }
                };
                let id = st.connection;
                let Some(session) = self.sessions.get(&id).cloned() else {
                    self.status = "la conexión no está activa".to_string();
                    return Task::none();
                };
                if let Some(st) = &mut self.users {
                    st.confirm_drop = None;
                    st.running = true;
                }
                self.status = "eliminando usuario…".to_string();
                users_exec_task(
                    self.rt.clone(),
                    session.client.clone(),
                    sql,
                    "usuario eliminado".to_string(),
                )
            }
            Message::UsersField(field, value) => {
                if let Some(edit) = self.users.as_mut().and_then(|st| st.edit.as_mut()) {
                    match field {
                        UserField::Name => edit.name = value,
                        UserField::Password => edit.password = value,
                        UserField::Host => edit.host = value,
                    }
                }
                Task::none()
            }
            Message::UsersToggleLogin => {
                if let Some(edit) = self.users.as_mut().and_then(|st| st.edit.as_mut()) {
                    edit.can_login = !edit.can_login;
                }
                Task::none()
            }
            Message::UsersToggleSuper => {
                if let Some(edit) = self.users.as_mut().and_then(|st| st.edit.as_mut()) {
                    edit.superuser = !edit.superuser;
                }
                Task::none()
            }
            Message::UsersRun => {
                let Some(st) = &self.users else {
                    return Task::none();
                };
                if st.running {
                    return Task::none();
                }
                let Some(edit) = &st.edit else {
                    return Task::none();
                };
                let sql = match user_ddl(st.engine, edit.action(), edit) {
                    Ok(s) => s,
                    Err(e) => {
                        self.status = format!("error: {e}");
                        return Task::none();
                    }
                };
                let done = if edit.action() == UserAction::Alter {
                    "usuario actualizado".to_string()
                } else {
                    "usuario creado".to_string()
                };
                let id = st.connection;
                let Some(session) = self.sessions.get(&id).cloned() else {
                    self.status = "la conexión no está activa".to_string();
                    return Task::none();
                };
                if let Some(st) = &mut self.users {
                    st.running = true;
                    st.error = None;
                }
                self.status = "ejecutando DDL…".to_string();
                users_exec_task(self.rt.clone(), session.client.clone(), sql, done)
            }
            Message::UsersApplied(res) => {
                let Some(st) = &mut self.users else {
                    return Task::none();
                };
                st.running = false;
                match res {
                    Ok(done) => {
                        st.edit = None;
                        st.error = None;
                        self.status = format!("✓ {done}");
                        let (engine, id) = (st.engine, st.connection);
                        let Some(session) = self.sessions.get(&id).cloned() else {
                            st.loading = false;
                            return Task::none();
                        };
                        st.loading = true;
                        users_list_task(self.rt.clone(), session.client.clone(), engine)
                    }
                    Err(e) => {
                        st.error = Some(e.clone());
                        self.status = format!("falló: {e}");
                        Task::none()
                    }
                }
            }
            Message::EditorSave => {
                let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
                std::thread::spawn(move || {
                    let mut dialog = rfd::FileDialog::new()
                        .set_file_name("consulta.sql")
                        .add_filter("SQL", &["sql"]);
                    if let Some(docs) = dirs::document_dir() {
                        dialog = dialog.set_directory(docs);
                    }
                    let _ = tx.send(dialog.save_file());
                });
                Task::perform(async move { rx.await.unwrap_or(None) }, Message::EditorSaveTo)
            }
            Message::EditorSaveTo(Some(path)) => {
                let sql = self.editor.content();
                let display = path.display().to_string();
                let shown = display.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::write(&path, sql)
                                .map_err(|e| format!("no se pudo escribir «{shown}»: {e}"))
                        })
                        .await
                        .map_err(|e| e.to_string())?
                        .map(|()| display)
                    },
                    Message::EditorSaved,
                )
            }
            Message::EditorSaveTo(None) => {
                self.status = "guardado cancelado".to_string();
                Task::none()
            }
            Message::EditorSaved(Ok(ruta)) => {
                self.status = format!("✓ guardado: {ruta}");
                Task::none()
            }
            Message::EditorSaved(Err(e)) => {
                self.status = format!("error: {e}");
                Task::none()
            }
            Message::EditorOpen => {
                let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
                std::thread::spawn(move || {
                    let mut dialog = rfd::FileDialog::new().add_filter("SQL", &["sql", "txt"]);
                    if let Some(docs) = dirs::document_dir() {
                        dialog = dialog.set_directory(docs);
                    }
                    let _ = tx.send(dialog.pick_file());
                });
                Task::perform(async move { rx.await.unwrap_or(None) }, Message::EditorOpenTo)
            }
            Message::EditorOpenTo(Some(path)) => {
                let display = path.display().to_string();
                let shown = display.clone();
                self.status = "leyendo…".to_string();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::read_to_string(&path)
                                .map_err(|e| format!("no se pudo leer «{shown}»: {e}"))
                        })
                        .await
                        .map_err(|e| e.to_string())?
                        .map(|content| (display, content))
                    },
                    Message::EditorOpened,
                )
            }
            Message::EditorOpenTo(None) => {
                self.status = "apertura cancelada".to_string();
                Task::none()
            }
            Message::EditorOpened(Ok((ruta, content))) => {
                self.status = format!("✓ abierto: {ruta}");
                let task = self.editor.reset(&content).map(Message::Editor);
                self.stash_tab();
                task
            }
            Message::EditorOpened(Err(e)) => {
                self.status = format!("error: {e}");
                Task::none()
            }
            Message::HistoryOpen => {
                self.history_panel = Some(HistoryPanel {
                    filter: String::new(),
                    clear_confirm: false,
                });
                Task::none()
            }
            Message::HistoryClose => {
                self.history_panel = None;
                Task::none()
            }
            Message::HistoryFilter(s) => {
                if let Some(panel) = &mut self.history_panel {
                    panel.filter = s;
                }
                Task::none()
            }
            Message::HistoryPick(i) => {
                let Some(entry) = self.history.get(i).cloned() else {
                    return Task::none();
                };
                self.history_panel = None;
                self.status = "SQL cargado en el editor".to_string();
                let task = self.editor.reset(&entry.sql).map(Message::Editor);
                self.stash_tab();
                task
            }
            Message::HistoryClearAsk => {
                if let Some(panel) = &mut self.history_panel {
                    panel.clear_confirm = true;
                }
                Task::none()
            }
            Message::HistoryClearNo => {
                if let Some(panel) = &mut self.history_panel {
                    panel.clear_confirm = false;
                }
                Task::none()
            }
            Message::HistoryClearYes => {
                self.history.clear();
                store::save_history(&self.history);
                if let Some(panel) = &mut self.history_panel {
                    panel.clear_confirm = false;
                }
                self.status = "historial borrado".to_string();
                Task::none()
            }
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
                if self.objects.as_ref().is_some_and(|o| o.container.connection == removed.cfg.id) {
                    self.objects = None;
                }
                if self.data_edit.as_ref().is_some_and(|e| e.connection == removed.cfg.id) {
                    self.data_edit = None;
                    self.data_pending = None;
                }
                if self.import.as_ref().is_some_and(|s| s.connection == removed.cfg.id) {
                    self.import = None;
                }
                if self.users.as_ref().is_some_and(|s| s.connection == removed.cfg.id) {
                    self.users = None;
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
                let client = session.client.clone();
                let sql = self.editor.content();
                if sql.trim().is_empty() {
                    self.status = "el editor está vacío".to_string();
                    return Task::none();
                }
                // The history records every statement launched from Run/F5.
                let conn_name = self
                    .entries
                    .iter()
                    .find(|e| e.cfg.id == id)
                    .map(|e| e.cfg.name.clone());
                self.history_push(sql.clone(), conn_name);
                // Data editing arms only for the exact SELECT generated for
                // a table (a hand-edited SQL stays read-only).
                let data_obj = match self.selected_object.as_ref() {
                    Some((obj, ObjectKind::Table))
                        if self.query_for(obj, ObjectKind::Table) == sql =>
                    {
                        Some((obj.clone(), sql.clone()))
                    }
                    _ => None,
                };
                self.data_pending = data_obj;
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
                let now = std::time::Instant::now();
                let dbl = is_double_click(
                    self.tree_last_click.as_ref(),
                    &TreeClickTarget::Node(node.clone()),
                    now,
                );
                self.tree_last_click = Some((TreeClickTarget::Node(node.clone()), now));
                // A plain click dismisses any context menu.
                self.context_menu = None;
                // Expanding a database/schema also selects it: it seeds the
                // parent for "Nueva tabla" and shows where you are.
                if matches!(kind, ObjectKind::Database | ObjectKind::Schema) {
                    self.selected_object = Some((node.clone(), kind));
                }
                let expand = self.expand(&node);
                if dbl {
                    // Second quick click on the same database/schema: list
                    // its children in the objects panel (Navicat double-click).
                    Task::batch([expand, self.update(Message::ObjectsOpen(node, kind))])
                } else {
                    expand
                }
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
                    self.data_pending = None;
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
        tab.data_edit = self.data_edit.take();
        // A pending data SELECT belongs to the tab that launched it.
        self.data_pending = None;
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
        self.data_edit = tab.data_edit.take();
        self.data_pending = None;
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
            // Data-edit strip: install when this landing result is the
            // armed generated SELECT (parked results stay read-only).
            let armed = if result.is_rows() {
                self.data_pending.take()
            } else {
                self.data_pending = None;
                None
            };
            self.data_edit = None;
            let edit_task = match armed {
                Some((obj, sql)) => {
                    let (edit_columns, edit_rows) =
                        (result.columns.clone(), result.rows.clone());
                    self.data_edit = Some(DataEditState {
                        source: obj.clone(),
                        connection: obj.connection,
                        sql,
                        pk: Vec::new(),
                        loading_pk: true,
                        columns: edit_columns,
                        rows: edit_rows,
                        selected: None,
                        insert: None,
                        confirm_delete: false,
                        drafts: HashMap::new(),
                        deletes: HashSet::new(),
                    });
                    match self.sessions.get(&obj.connection) {
                        Some(session) => {
                            data_describe_task(self.rt.clone(), session.client.clone(), obj)
                        }
                        None => {
                            self.data_edit = None;
                            Task::none()
                        }
                    }
                }
                None => Task::none(),
            };
            let (grid, init) = grid::build(result);
            self.grid = grid;
            Task::batch([init.map(Message::Grid), edit_task])
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
        self.objects = None;
        self.import = None;
        self.users = None;
        self.data_edit = None;
        self.data_pending = None;
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

    /// Objects panel: Navicat-style list of a container's children with a
    /// toolbar (Recargar / Nueva tabla / Diseñar / Eliminar / Volver) and a
    /// search box. Double-click (by time) opens a row as a query tab;
    /// Database/Schema rows drill the panel deeper.
    fn objects_view<'a>(&'a self, o: &'a ObjectsState) -> Element<'a, Message> {
        let engine = self.engine_of(o.container.connection);
        let sql_db = engine != Engine::Mongodb;
        let rows = filtered_nodes(&self.tree, &o.container, &o.filter);
        let sel = o.selected.and_then(|i| rows.get(i).copied());
        let title = o.container.path.last().cloned().unwrap_or_else(|| {
            // Connection root: show the entry's name.
            self.entries
                .iter()
                .find(|e| e.cfg.id == o.container.connection)
                .map(|e| e.cfg.name.clone())
                .unwrap_or_default()
        });

        let act_btn = |icon: icons::Icon,
                       label: &'static str,
                       act: ObjectsAction,
                       enabled: bool|
         -> Element<'static, Message> {
            button(
                row![icons::svg(icon, 14.0), text(label).size(12)]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
            )
            .padding([4.0, 8.0])
            .style(theme::flat)
            .on_press_maybe(enabled.then_some(Message::ObjectsAction(act)))
            .into()
        };

        let strip = container(
            row![
                icons::svg(icons::Icon::for_kind(o.kind), 16.0),
                text(format!("{} · {}", kind_label(o.kind), title)).size(13),
                act_btn(
                    icons::Icon::Refresh,
                    "Recargar",
                    ObjectsAction::Refresh,
                    true
                ),
                act_btn(
                    icons::Icon::NewTable,
                    "Nueva tabla",
                    ObjectsAction::NewTable,
                    sql_db
                        && matches!(o.kind, ObjectKind::Database | ObjectKind::Schema),
                ),
                act_btn(
                    icons::Icon::Design,
                    "Diseñar",
                    ObjectsAction::Design,
                    sql_db && sel.is_some_and(|n| n.kind == ObjectKind::Table),
                ),
                act_btn(
                    icons::Icon::Import,
                    "Importar",
                    ObjectsAction::Import,
                    sql_db && sel.is_some_and(|n| n.kind == ObjectKind::Table),
                ),
                act_btn(
                    icons::Icon::Delete,
                    "Eliminar",
                    ObjectsAction::Delete,
                    sql_db
                        && sel
                            .is_some_and(|n| matches!(n.kind, ObjectKind::Table | ObjectKind::View)),
                ),
                button(text("✕ Volver").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::ObjectsClose),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            right: 6.0,
            bottom: 6.0,
            left: 6.0,
        })
        .style(theme::chrome);

        // Two-step delete: the armed confirmation replaces the search row.
        let filter_row: Element<'_, Message> = if o.confirm {
            let name = sel.map(|n| n.label.clone()).unwrap_or_default();
            row![
                text(format!(
                    "¿Eliminar «{name}»? Esta acción no se puede deshacer."
                ))
                .size(12),
                button(text("Sí, eliminar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::solid)
                    .on_press(Message::ObjectsAction(ObjectsAction::ConfirmDelete)),
                button(text("Cancelar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::ObjectsAction(ObjectsAction::CancelDelete)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            row![
                icons::svg(icons::Icon::Search, 13.0),
                text_input("Buscar objeto…", &o.filter)
                    .on_input(Message::ObjectsFilter)
                    .size(12)
                    .padding([5.0, 7.0]),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        };

        let mut list: Column<'_, Message> = Column::new().spacing(1);
        if rows.is_empty() {
            let msg = if !self.tree.contains_key(&o.container) {
                "cargando objetos…"
            } else if o.filter.trim().is_empty() {
                "sin objetos en este contenedor"
            } else {
                "sin coincidencias"
            };
            list = list.push(text(msg).size(12).color(theme::muted()));
        }
        for (i, node) in rows.iter().enumerate() {
            let style: fn(&Theme, button::Status) -> button::Style = if o.selected == Some(i) {
                theme::row_selected
            } else {
                theme::flat
            };
            let mut label = row![
                icons::svg(icons::Icon::for_kind(node.kind), 14.0),
                text(node.label.clone()).size(13),
                Space::new().width(Length::Fill),
                text(kind_label(node.kind)).size(11).color(theme::muted()),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            if let Some(detail) = &node.detail {
                label = label.push(text(detail.clone()).size(11).color(theme::muted()));
            }
            list = list.push(
                button(container(label).padding([3.0, 8.0]))
                    .width(Length::Fill)
                    .style(style)
                    .on_press(Message::ObjectsRowClick(i)),
            );
        }

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(
                container(filter_row)
                    .padding([6.0, 8.0])
                    .width(Length::Fill)
                    .style(theme::chrome),
            )
            .push(scrollable(container(list).padding([4.0, 8.0])).height(Length::Fill))
            .width(Length::Fill)
            .into()
    }

    /// Data-edit strip between the result header and the grid: toolbar
    /// (apply / insert / delete / cancel, or the two-step delete confirm)
    /// plus a horizontal form over the selected row's fields. `None` when
    /// no table edit is armed (the grid keeps its normal layout).
    fn data_edit_view(&self) -> Option<Element<'_, Message>> {
        let edit = self.data_edit.as_ref()?;
        let table = edit
            .source
            .path
            .last()
            .cloned()
            .unwrap_or_else(|| "tabla".to_string());
        let changes = edit.pending_count();
        let ready = !edit.loading_pk;
        let sel = edit.selected;
        let inserting = edit.insert.is_some();

        let hint = if edit.loading_pk {
            "leyendo clave primaria…".to_string()
        } else if inserting {
            "fila nueva".to_string()
        } else {
            sel.map(|i| format!("fila #{i}"))
                .unwrap_or_else(|| "selecciona una fila".to_string())
        };

        // ── toolbar: apply/insert/delete/cancel or delete confirmation ──
        let actions: Element<'_, Message> = if edit.confirm_delete {
            row![
                text(format!(
                    "¿Marcar para eliminar la fila #{}?",
                    sel.unwrap_or(0)
                ))
                .size(12),
                button(text("Sí, marcar").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::solid)
                    .on_press(Message::DataDeleteConfirm),
                button(text("Cancelar").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::flat)
                    .on_press(Message::DataDeleteCancel),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            row![
                button(text("Aplicar").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::solid)
                    .on_press_maybe((ready && changes > 0).then_some(Message::DataApply)),
                button(text("Insertar fila").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::flat)
                    .on_press_maybe(ready.then_some(Message::DataStartInsert)),
                button(text("Borrar fila").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::flat)
                    .on_press_maybe(
                        (ready && sel.is_some() && !inserting).then_some(Message::DataDeleteAsk),
                    ),
                button(text("Cancelar").size(12))
                    .padding([3.0, 7.0])
                    .style(theme::flat)
                    .on_press_maybe((changes > 0).then_some(Message::DataCancel)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        };

        let bar = container(
            row![
                text(format!("✎ {table}")).size(12),
                text(hint).size(12).color(theme::muted()),
                text(format!("{changes} cambio(s) pendiente(s)")).size(12),
                container(actions)
                    .width(Length::Fill)
                    .align_x(iced::Alignment::End),
                button(text("✕").size(11))
                    .padding([3.0, 6.0])
                    .style(theme::flat)
                    .on_press(Message::DataClose),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([5.0, 8.0])
        .style(theme::chrome);

        // ── body: waiting / field form (insert row or selected row) ─────
        let body: Element<'_, Message> = if edit.loading_pk {
            text("leyendo claves primarias…")
                .size(12)
                .color(theme::muted())
                .into()
        } else {
            let mut fields = row![].spacing(8).align_y(iced::Alignment::Start);
            if inserting {
                for (idx, meta) in edit.columns.iter().enumerate() {
                    let draft = edit.insert_draft(idx);
                    fields = fields.push(field_block(
                        &meta.name,
                        false,
                        false,
                        meta.data_type.family == TypeFamily::Bytes,
                        draft,
                        move |v| Message::DataInsertField(idx, v),
                        move |v| Message::DataInsertFieldNull(idx, v),
                    ));
                }
            } else if let Some(row_idx) = sel.filter(|r| *r < edit.rows.len()) {
                let orig_row = &edit.rows[row_idx];
                for (idx, meta) in edit.columns.iter().enumerate() {
                    let draft = edit.draft(row_idx, idx);
                    let dirty = orig_row
                        .get(idx)
                        .is_some_and(|orig| !draft.matches_cell(orig));
                    let is_pk = edit
                        .pk
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&meta.name));
                    fields = fields.push(field_block(
                        &meta.name,
                        is_pk,
                        dirty,
                        meta.data_type.family == TypeFamily::Bytes,
                        draft,
                        move |v| Message::DataField(row_idx, idx, v),
                        move |v| Message::DataFieldNull(row_idx, idx, v),
                    ));
                }
            } else {
                fields = fields.push(
                    text("haz clic en una fila del grid para editarla")
                        .size(12)
                        .color(theme::muted()),
                );
            }
            scrollable(fields)
                .horizontal()
                .height(Length::Fixed(96.0))
                .into()
        };

        Some(
            Column::new()
                .push(bar)
                .push(rule::horizontal(1).style(theme::hairline))
                .push(
                    container(body)
                        .width(Length::Fill)
                        .padding([6.0, 8.0])
                        .style(theme::chrome),
                )
                .into(),
        )
    }

    /// Open the users/roles overlay for a connection: read-only listing at
    /// first; create/alter preview and run their DDL afterwards. SQLite has
    /// no users, MongoDB manages DB roles and Oracle is deferred (v1).
    fn users_open(&mut self, id: ConnectionId) -> Task<Message> {
        let engine = self.engine_of(id);
        if matches!(engine, Engine::Sqlite | Engine::Mongodb | Engine::Oracle) {
            self.status = match engine {
                Engine::Sqlite => "SQLite no tiene usuarios: la base es el archivo".to_string(),
                Engine::Mongodb => {
                    "MongoDB gestiona roles de base de datos: no disponible aún".to_string()
                }
                _ => "gestión de usuarios de Oracle postergada".to_string(),
            };
            return Task::none();
        }
        let Some(session) = self.sessions.get(&id).cloned() else {
            self.status = "sin sesión: selecciónala de nuevo para ver usuarios".to_string();
            return Task::none();
        };
        self.context_menu = None;
        self.users = Some(UsersState::new(id, engine));
        self.status = "leyendo usuarios…".to_string();
        users_list_task(self.rt.clone(), session.client.clone(), engine)
    }

    /// Users overlay: the account list, or the create/alter form with its
    /// inline DDL preview (same pane style as the sync plan).
    fn users_view<'a>(&'a self, st: &'a UsersState) -> Element<'a, Message> {
        let conn_name = self
            .entries
            .iter()
            .find(|e| e.cfg.id == st.connection)
            .map(|e| e.cfg.name.clone())
            .unwrap_or_else(|| "conexión".to_string());

        let strip = container(
            row![
                text(format!("  Usuarios · {conn_name}")).size(13),
                if st.loading {
                    text("cargando…").size(11).color(theme::muted())
                } else {
                    text("").size(11)
                },
                Space::new().width(Length::Fill),
                button(text("Crear…").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press_maybe(
                        (st.can_mutate() && st.edit.is_none() && !st.running && !st.loading)
                            .then_some(Message::UsersCreate),
                    ),
                button(text("Actualizar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press_maybe(
                        (st.edit.is_none() && !st.running && !st.loading)
                            .then_some(Message::UsersRefresh),
                    ),
                button(text("✕ Cerrar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::UsersClose),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            right: 6.0,
            bottom: 6.0,
            left: 6.0,
        })
        .style(theme::chrome);

        let mut body = Column::new().spacing(10);
        if let Some(edit) = &st.edit {
            let action = edit.action();
            let ddl = user_ddl(st.engine, action, edit);

            let mut name_row = row![
                text("Nombre:").size(11).color(theme::muted()),
                text_input("usuario", &edit.name)
                    .on_input(|v| Message::UsersField(UserField::Name, v))
                    .size(12)
                    .width(220.0),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            if matches!(st.engine, Engine::Mysql | Engine::MariaDb) {
                name_row = name_row
                    .push(text("Host:").size(11).color(theme::muted()))
                    .push(
                        text_input("%", &edit.host)
                            .on_input(|v| Message::UsersField(UserField::Host, v))
                            .size(12)
                            .width(150.0),
                    );
            }
            body = body.push(name_row);

            let mut pwd_row = row![
                text("Contraseña:").size(11).color(theme::muted()),
                text_input("vacía = sin cambio", &edit.password)
                    .on_input(|v| Message::UsersField(UserField::Password, v))
                    .size(12)
                    .width(220.0)
                    .secure(true),
                text("Puede entrar").size(12),
                checkbox(edit.can_login)
                    .size(13)
                    .on_toggle(|_| Message::UsersToggleLogin),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            if st.engine == Engine::Postgres {
                pwd_row = pwd_row
                    .push(text("Superusuario").size(12))
                    .push(
                        checkbox(edit.superuser)
                            .size(13)
                            .on_toggle(|_| Message::UsersToggleSuper),
                    );
            }
            body = body.push(pwd_row);

            let preview_sql = match &ddl {
                Ok(sql) => sql.clone(),
                Err(e) => format!("✗ {e}"),
            };
            body = body.push(
                container(
                    scrollable(text(preview_sql).size(12).font(iced::Font::MONOSPACE))
                        .height(Length::Fixed(150.0)),
                )
                .padding(10)
                .style(theme::workspace),
            );

            let action_label = match action {
                UserAction::Create => "Ejecutar DDL",
                UserAction::Alter => "Aplicar cambios",
            };
            body = body.push(
                row![
                    button(text(action_label).size(12))
                        .padding([4.0, 10.0])
                        .style(theme::solid)
                        .on_press_maybe((ddl.is_ok() && !st.running).then_some(Message::UsersRun)),
                    button(text("Cancelar").size(12))
                        .padding([4.0, 10.0])
                        .style(theme::flat)
                        .on_press(Message::UsersEditCancel),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
            if st.running {
                body = body.push(text("ejecutando…").size(11).color(theme::muted()));
            }
            if let Some(err) = &st.error {
                body = body.push(text(format!("✗ {err}")).size(12));
            }
        } else {
            if let Some(err) = &st.error {
                body = body.push(text(format!("✗ {err}")).size(12));
            }
            if st.loading {
                body = body.push(text("leyendo cuentas…").size(12).color(theme::muted()));
            } else if st.users.is_empty() {
                body = body.push(
                    text("sin cuentas visibles (¿permisos de catálogo?)")
                        .size(12)
                        .color(theme::muted()),
                );
            } else {
                body = body.push(
                    container(
                        row![
                            container(text("Nombre").size(11).color(theme::muted()))
                                .width(220.0),
                            container(text("Detalle").size(11).color(theme::muted()))
                                .width(200.0),
                            Space::new().width(Length::Fill),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding([2.0, 4.0]),
                );
                for row in &st.users {
                    let confirm = st.confirm_drop.as_deref() == Some(row.name.as_str());
                    let gated = st.can_mutate() && !st.running && st.edit.is_none();
                    let mut actions = row![].spacing(6).align_y(iced::Alignment::Center);
                    if confirm {
                        actions = actions
                            .push(text("¿Eliminar?").size(12))
                            .push(
                                button(text("Sí, eliminar").size(12))
                                    .padding([2.0, 6.0])
                                    .style(theme::flat)
                                    .on_press_maybe(
                                        gated.then_some(Message::UsersDropConfirm(row.name.clone())),
                                    ),
                            )
                            .push(
                                button(text("No").size(12))
                                    .padding([2.0, 6.0])
                                    .style(theme::flat)
                                    .on_press(Message::UsersDropCancel),
                            );
                    } else {
                        actions = actions
                            .push(
                                button(text("Modificar…").size(12))
                                    .padding([2.0, 6.0])
                                    .style(theme::flat)
                                    .on_press_maybe(
                                        gated.then_some(Message::UsersAlter(row.name.clone())),
                                    ),
                            )
                            .push(
                                button(text("Eliminar…").size(12))
                                    .padding([2.0, 6.0])
                                    .style(theme::flat)
                                    .on_press_maybe(
                                        gated.then_some(Message::UsersDropAsk(row.name.clone())),
                                    ),
                            );
                    }
                    body = body.push(
                        container(
                            row![
                                container(text(row.name.as_str()).size(12)).width(220.0),
                                container(text(row.detail.as_str()).size(12).color(theme::muted()))
                                    .width(200.0),
                                Space::new().width(Length::Fill),
                                actions,
                            ]
                            .spacing(6)
                            .align_y(iced::Alignment::Center),
                        )
                        .padding([3.0, 4.0]),
                    );
                }
            }
            if !st.can_mutate() {
                body = body.push(
                    text("solo consulta en este motor (v1: MySQL y PostgreSQL admiten cambios)")
                        .size(11)
                        .color(theme::muted()),
                );
            }
        }

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(scrollable(container(body).padding(10)).height(Length::Fill))
            .width(Length::Fill)
            .into()
    }

    /// Record the statement launched from Run/F5 and persist the history.
    fn history_push(&mut self, sql: String, conn: Option<String>) {
        history_push(&mut self.history, sql, conn);
        store::save_history(&self.history);
    }

    /// Query-history overlay: filterable list of executed statements with
    /// "Cargar" (into the editor) and a wipe action.
    fn history_view<'a>(&'a self, panel: &'a HistoryPanel) -> Element<'a, Message> {
        let clear: Element<'_, Message> = if panel.clear_confirm {
            row![
                text("¿Borrar todo?").size(12),
                button(text("Sí, borrar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::HistoryClearYes),
                button(text("No").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::HistoryClearNo),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            button(text("Limpiar…").size(12))
                .padding([4.0, 8.0])
                .style(theme::flat)
                .on_press_maybe((!self.history.is_empty()).then_some(Message::HistoryClearAsk))
                .into()
        };

        let strip = container(
            row![
                text("  Historial").size(13),
                text(format!("{} entrada(s)", self.history.len()))
                    .size(11)
                    .color(theme::muted()),
                Space::new().width(Length::Fill),
                text_input("filtrar SQL…", &panel.filter)
                    .on_input(|v| Message::HistoryFilter(v))
                    .size(12)
                    .width(200.0),
                clear,
                button(text("✕ Cerrar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press(Message::HistoryClose),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            right: 6.0,
            bottom: 6.0,
            left: 6.0,
        })
        .style(theme::chrome);

        let filter = panel.filter.trim().to_lowercase();
        let mut body = Column::new().spacing(4);
        if self.history.is_empty() {
            body = body.push(
                text("aún no hay consultas ejecutadas (F5 o «Ejecutar»)")
                    .size(12)
                    .color(theme::muted()),
            );
        } else {
            let mut shown = 0;
            for (i, entry) in self.history.iter().enumerate() {
                if !filter.is_empty() && !entry.sql.to_lowercase().contains(&filter) {
                    continue;
                }
                shown += 1;
                body = body.push(
                    container(
                        row![
                            container(text(entry.when.as_str()).size(11).color(theme::muted()))
                                .width(86.0),
                            container(
                                text(entry.conn.as_deref().unwrap_or("—"))
                                    .size(11)
                                    .color(theme::muted())
                            )
                            .width(130.0),
                            text(history_line(&entry.sql)).size(12),
                            Space::new().width(Length::Fill),
                            button(text("Cargar").size(12))
                                .padding([2.0, 6.0])
                                .style(theme::flat)
                                .on_press(Message::HistoryPick(i)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding([3.0, 4.0]),
                );
            }
            if shown == 0 {
                body = body.push(text("sin coincidencias…").size(12).color(theme::muted()));
            }
        }

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(scrollable(container(body).padding(10)).height(Length::Fill))
            .width(Length::Fill)
            .into()
    }

    /// Open the import wizard for a table: installs the state, reads the
    /// table's columns and opens the native file dialog.
    fn import_start(&mut self, table: ObjectRef) -> Task<Message> {
        let engine = self.engine_of(table.connection);
        if engine == Engine::Mongodb {
            self.status = "importar no está disponible en MongoDB".to_string();
            return Task::none();
        }
        let Some(session) = self.sessions.get(&table.connection).cloned() else {
            self.status = "sin sesión: no se puede importar".to_string();
            return Task::none();
        };
        self.context_menu = None;
        self.import = Some(ImportState::new(table.clone(), table.connection));
        let describe =
            import_describe_task(self.rt.clone(), session.client.clone(), table);
        Task::batch([describe, import_dialog_task()])
    }

    /// Re-read + re-parse the file with the current wizard settings.
    fn import_reload(&mut self) -> Task<Message> {
        let Some(st) = &self.import else {
            return Task::none();
        };
        let path = st.path.trim().to_string();
        if path.is_empty() {
            return Task::none();
        }
        let (format, delimiter, header) = (st.format, st.delimiter, st.header);
        if let Some(st) = &mut self.import {
            st.error = None;
            st.rows.clear();
            st.file_columns.clear();
            st.mapping.clear();
        }
        self.status = "cargando el fichero…".to_string();
        import_load_task(path, format, delimiter, header)
    }

    /// Import wizard overlay: file picker, format options, column mapping,
    /// preview and run (replaces the editor + results while open).
    fn import_view<'a>(&'a self, st: &'a ImportState) -> Element<'a, Message> {
        let table = st
            .table
            .path
            .last()
            .cloned()
            .unwrap_or_else(|| "tabla".to_string());
        let red = iced::Color::from_rgb(0.898, 0.325, 0.294);

        // ── header strip ────────────────────────────────────────────────
        let can_run = !st.loading
            && !st.running
            && st.error.is_none()
            && !st.rows.is_empty()
            && st.mapped_count() > 0;
        let strip = container(
            row![
                icons::svg(icons::Icon::Import, 16.0),
                text(format!("Importar en: {table}")).size(13),
                container(
                    row![
                        button(text("✓ Importar").size(12))
                            .style(theme::solid)
                            .on_press_maybe(can_run.then_some(Message::ImportRun)),
                        button(text("✕ Volver").size(12))
                            .style(theme::flat)
                            .on_press(Message::ImportClose),
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
        .padding(iced::Padding {
            top: 6.0,
            right: 6.0,
            bottom: 0.0,
            left: 6.0,
        })
        .style(theme::chrome);

        // ── body: file row, format row, status, mapping, preview ────────
        let mut body = Column::new().spacing(10);

        body = body.push(
            row![
                text("Fichero:").size(11).color(theme::muted()),
                text_input("ruta/al/fichero.csv", &st.path)
                    .on_input(Message::ImportSetPath)
                    .size(12)
                    .width(Length::Fill),
                button(text("Examinar…").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press_maybe(
                        (!st.running).then_some(Message::ImportBrowse),
                    ),
                button(text("Cargar").size(12))
                    .padding([4.0, 8.0])
                    .style(theme::flat)
                    .on_press_maybe(
                        (!st.path.trim().is_empty() && !st.running)
                            .then_some(Message::ImportReload),
                    ),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );

        let mut fmt = row![
            text("Formato:").size(11).color(theme::muted()),
            pick_list(
                ImportFormat::ALL,
                Some(st.format),
                Message::ImportFormatSel
            ),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        if st.format == ImportFormat::Csv {
            fmt = fmt
                .push(text("Delimitador:").size(11).color(theme::muted()))
                .push(pick_list(
                    DELIMS,
                    Some(Delim(st.delimiter)),
                    |d| Message::ImportDelimiter(d.0),
                ));
        }
        if st.format != ImportFormat::Json {
            fmt = fmt
                .push(
                    checkbox(st.header)
                        .size(13)
                        .on_toggle(Message::ImportHeader),
                )
                .push(text("Primera fila = nombres").size(11));
        }
        body = body.push(fmt);

        let status_line: Element<'_, Message> = if let Some(err) = &st.error {
            text(format!("⚠ {err}")).size(12).color(red).into()
        } else if st.loading {
            text("leyendo la estructura de la tabla…")
                .size(12)
                .color(theme::muted())
                .into()
        } else if st.running {
            text("importando…").size(12).color(theme::muted()).into()
        } else if let Some(dup) = st.duplicate_target() {
            text(format!("⚠ {dup}")).size(12).color(red).into()
        } else {
            text(format!(
                "{} filas × {} columnas · {} mapeada(s)",
                st.rows.len(),
                st.file_columns.len(),
                st.mapped_count()
            ))
            .size(12)
            .color(theme::muted())
            .into()
        };
        body = body.push(status_line);

        if !st.loading && !st.file_columns.is_empty() {
            let head = row![
                text("Columna del fichero")
                    .size(11)
                    .color(theme::muted())
                    .width(Length::Fixed(190.0)),
                Space::new().width(Length::Fixed(24.0)),
                text("Columna de la tabla")
                    .size(11)
                    .color(theme::muted())
                    .width(Length::Fixed(190.0)),
                text("Muestra (1ª fila)").size(11).color(theme::muted()),
            ]
            .spacing(6);
            let mut map_list = Column::new().spacing(4).push(head);
            for (i, file_col) in st.file_columns.iter().enumerate() {
                let selected = st.mapping.get(i).copied().flatten();
                let options: Vec<MapChoice> =
                    std::iter::once(MapChoice(None, "(omitir)".to_string()))
                        .chain(st.columns.iter().enumerate().map(|(t, c)| {
                            MapChoice(Some(t), c.name.clone())
                        }))
                        .collect();
                let sel_label = selected
                    .and_then(|t| st.columns.get(t))
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "(omitir)".to_string());
                let mut sample = st
                    .rows
                    .first()
                    .and_then(|r| r.get(i))
                    .cloned()
                    .unwrap_or_default();
                if sample.chars().count() > 40 {
                    sample = sample.chars().take(40).collect::<String>() + "…";
                }
                map_list = map_list.push(
                    row![
                        text(file_col.clone())
                            .size(12)
                            .width(Length::Fixed(190.0)),
                        text("→")
                            .size(11)
                            .color(theme::muted())
                            .width(Length::Fixed(24.0)),
                        pick_list(
                            options,
                            Some(MapChoice(selected, sel_label)),
                            move |choice| Message::ImportMap(i, choice.0),
                        )
                        .width(Length::Fixed(190.0)),
                        text(sample).size(11).color(theme::muted()),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                );
            }
            body = body.push(map_list);
        }

        if !st.rows.is_empty() && st.error.is_none() {
            body = body.push(text("Vista previa:").size(11).color(theme::muted()));
            let mut preview = Column::new().spacing(2);
            for row_vals in st.rows.iter().take(5) {
                let mut line = row_vals.join(" | ");
                if line.chars().count() > 160 {
                    line = line.chars().take(160).collect::<String>() + "…";
                }
                preview = preview.push(text(line).size(10).color(theme::muted()));
            }
            body = body.push(
                container(preview)
                    .padding([6.0, 8.0])
                    .width(Length::Fill)
                    .style(theme::workspace),
            );
        }

        Column::new()
            .push(strip)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(
                container(
                    scrollable(container(body).padding([12.0, 16.0]))
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::workspace),
            )
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
        tab_row = tab_row.push(Space::new().width(Length::Fill));
        tab_row = tab_row.push(
            button(
                row![icons::svg(icons::Icon::Open, 12.0), text(" Abrir…").size(12)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .style(theme::flat)
            .padding([3.0, 8.0])
            .on_press(Message::EditorOpen),
        );
        tab_row = tab_row.push(
            button(
                row![icons::svg(icons::Icon::Save, 12.0), text(" Guardar…").size(12)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .style(theme::flat)
            .padding([3.0, 8.0])
            .on_press(Message::EditorSave),
        );
        tab_row = tab_row.push(
            button(
                row![icons::svg(icons::Icon::Query, 12.0), text(" Historial").size(12)]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .style(theme::flat)
            .padding([3.0, 8.0])
            .on_press(Message::HistoryOpen),
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
                        text("Auto:").size(11).color(theme::muted()),
                        pick_list(
                            RefreshEvery::ALL,
                            Some(self.auto_refresh),
                            Message::AutoSet,
                        )
                        .text_size(12)
                        .width(76),
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

        // The users/history/import/sync/designer/ER/objects overlays replace
        // the editor + results while open (highest priority: users, lowest:
        // the objects panel).
        let center: Element<'_, Message> =
            match (
                &self.users,
                &self.history_panel,
                &self.import,
                &self.sync,
                &self.designer,
                &self.er,
                &self.objects,
            ) {
                (Some(u), _, _, _, _, _, _) => self.users_view(u),
                (None, Some(h), _, _, _, _, _) => self.history_view(h),
                (None, None, Some(imp), _, _, _, _) => self.import_view(imp),
                (None, None, None, Some(s), _, _, _) => self.sync_view(s),
                (None, None, None, None, Some(d), _, _) => self.designer_view(d),
                (None, None, None, None, None, Some(er), _) => self.er_view(er),
                (None, None, None, None, None, None, Some(o)) => self.objects_view(o),
                (None, None, None, None, None, None, None) => {
                    let mut col = Column::new()
                        .push(editor_tabs)
                        .push(editor.height(Length::FillPortion(2)))
                        .push(rule::horizontal(1).style(theme::hairline))
                        .push(result_strip);
                    if let Some(strip) = self.data_edit_view() {
                        col = col.push(strip);
                    }
                    col.push(grid.height(Length::FillPortion(3)))
                        .width(Length::Fill)
                        .into()
                }
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
                // Ctrl+S / Ctrl+O save/open the editor file (the editor
                // doesn't claim them, so they reach the global listener).
                iced::keyboard::Key::Character(ref c)
                    if control && !modifiers.shift() && c.as_ref() == "s" =>
                {
                    Some(Message::EditorSave)
                }
                iced::keyboard::Key::Character(ref c)
                    if control && !modifiers.shift() && c.as_ref() == "o" =>
                {
                    Some(Message::EditorOpen)
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
        let mut subs = vec![keys, window, events];
        if let Some(period) = self.auto_refresh.period() {
            subs.push(iced::time::every(period).map(|_| Message::AutoTick));
        }
        Subscription::batch(subs)
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

    let from = table_ref(engine, obj);

    if engine == Engine::SqlServer {
        format!("SELECT TOP 100 * FROM {from};")
    } else {
        format!("SELECT * FROM {from} LIMIT 100;")
    }
}

/// Target name for DML against `obj`, qualified exactly like the generated
/// SELECT is (SQLite is bare; MySQL `db.table`, Postgres `schema.table`,
/// SQL Server `db..table` for two-segment paths). Shared by the data-edit
/// UPDATE/INSERT/DELETE so DML can never diverge from the SELECT that
/// filled the grid.
fn table_ref(engine: Engine, obj: &ObjectRef) -> String {
    match engine {
        // The first segment is the file's display label, not a schema.
        Engine::Sqlite => quote_ident(
            obj.path.last().map(String::as_str).unwrap_or(""),
            engine,
        ),
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

/// Single-quoted text literal escaped for the engine. MySQL also escapes
/// backslashes (they are escape characters inside its strings); the other
/// engines only double the quote.
fn text_literal(engine: Engine, s: &str) -> String {
    let escaped = if matches!(engine, Engine::Mysql | Engine::MariaDb) {
        s.replace('\\', "\\\\").replace('\'', "''")
    } else {
        s.replace('\'', "''")
    };
    format!("'{escaped}'")
}

/// Boolean literal per dialect: Postgres spells TRUE/FALSE, the rest take 1/0.
fn bool_literal(engine: Engine, b: bool) -> String {
    match engine {
        Engine::Postgres => (if b { "TRUE" } else { "FALSE" }).to_string(),
        _ => (if b { "1" } else { "0" }).to_string(),
    }
}

/// Literal for a drafted form field: NULL wins when the toggle is set;
/// numeric families emit the raw number when it parses (plain, so no
/// coercion through a string) and everything else is a quoted text literal.
fn draft_literal(engine: Engine, family: TypeFamily, d: &FieldDraft) -> String {
    if d.null {
        return "NULL".to_string();
    }
    let t = d.text.trim();
    match family {
        TypeFamily::Int if t.parse::<i64>().is_ok() => t.to_string(),
        TypeFamily::UInt if t.parse::<u64>().is_ok() => t.to_string(),
        TypeFamily::Float | TypeFamily::Decimal if t.parse::<f64>().is_ok() => t.to_string(),
        TypeFamily::Bool => {
            let truthy = matches!(
                t.to_ascii_lowercase().as_str(),
                "true" | "1" | "t" | "yes" | "on"
            );
            bool_literal(engine, truthy)
        }
        _ => text_literal(engine, &d.text),
    }
}

/// Literal for an ORIGINAL fetched cell value (already typed): used inside
/// WHERE clauses so updates/deletes pin the row by its primary key as
/// read from the server. Binary cells are unreachable (rejected at install)
/// and fall back to their display text. `_family` documents the column the
/// value came from; the value itself carries enough typing to literalize.
fn cell_literal(engine: Engine, _family: TypeFamily, v: &CellValue) -> String {
    match v {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => bool_literal(engine, *b),
        CellValue::Int(i) => i.to_string(),
        CellValue::UInt(u) => u.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Text(s) => text_literal(engine, s),
        CellValue::DateTime(dt) => text_literal(engine, &dt.to_rfc3339()),
        CellValue::Json(j) => text_literal(engine, &j.to_string()),
        CellValue::Other { value, .. } => text_literal(engine, value),
        CellValue::Bytes(_) => text_literal(engine, &v.as_display()),
    }
}

/// Index of a result column by name, case-insensitively (PK names come from
/// the catalog while column names come from the result set).
fn col_index(columns: &[ColumnMeta], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
}

/// DML for a data-edit strip: one UPDATE per dirty row (SET only the dirty
/// columns, WHERE pins the ORIGINAL primary-key values), one INSERT per
/// pending new row (blank columns omitted so the engine applies
/// defaults/NULL) and one DELETE per marked row. Rows already marked for
/// deletion skip their UPDATEs. Returns `[]` when there is nothing to do.
fn edit_statements(engine: Engine, edit: &DataEditState) -> Vec<String> {
    let from = table_ref(engine, &edit.source);
    let pk_idx: Vec<usize> = edit
        .pk
        .iter()
        .filter_map(|name| col_index(&edit.columns, name))
        .collect();
    let pk_ok = pk_idx.len() == edit.pk.len() && !pk_idx.is_empty();
    let mut out = Vec::new();

    // ── UPDATEs (grouped by row, columns in stable index order) ────────
    if pk_ok {
        let mut by_row: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for &(row, col) in edit.drafts.keys() {
            by_row.entry(row).or_default().push(col);
        }
        for (row, mut cols) in by_row {
            if edit.deletes.contains(&row) || row >= edit.rows.len() {
                continue;
            }
            cols.sort_unstable();
            cols.dedup();
            let orig = &edit.rows[row];
            let sets: Vec<String> = cols
                .into_iter()
                .filter_map(|col| {
                    let d = edit.drafts.get(&(row, col))?;
                    let meta = edit.columns.get(col)?;
                    let cell = orig.get(col)?;
                    if d.matches_cell(cell) {
                        return None;
                    }
                    Some(format!(
                        "{} = {}",
                        quote_ident(&meta.name, engine),
                        draft_literal(engine, meta.data_type.family, d)
                    ))
                })
                .collect();
            if sets.is_empty() {
                continue;
            }
            let wheres: Vec<String> = pk_idx
                .iter()
                .map(|&i| {
                    let meta = &edit.columns[i];
                    format!(
                        "{} = {}",
                        quote_ident(&meta.name, engine),
                        cell_literal(engine, meta.data_type.family, &orig[i])
                    )
                })
                .collect();
            out.push(format!(
                "UPDATE {from} SET {} WHERE {};",
                sets.join(", "),
                wheres.join(" AND ")
            ));
        }
    }

    // ── INSERTs (only columns with a filled draft) ─────────────────────
    for ins in edit.insert.iter() {
        let mut names = Vec::new();
        let mut values = Vec::new();
        for (idx, meta) in edit.columns.iter().enumerate() {
            let Some(d) = ins.get(&idx) else {
                continue;
            };
            if !(d.null || !d.text.trim().is_empty()) {
                continue;
            }
            names.push(quote_ident(&meta.name, engine));
            values.push(draft_literal(engine, meta.data_type.family, d));
        }
        if names.is_empty() {
            out.push(if matches!(engine, Engine::Mysql | Engine::MariaDb) {
                format!("INSERT INTO {from} () VALUES ();")
            } else {
                format!("INSERT INTO {from} DEFAULT VALUES;")
            });
        } else {
            out.push(format!(
                "INSERT INTO {from} ({}) VALUES ({});",
                names.join(", "),
                values.join(", ")
            ));
        }
    }

    // ── DELETEs (marked rows, stable order) ────────────────────────────
    if pk_ok {
        let mut rows: Vec<usize> = edit.deletes.iter().copied().collect();
        rows.sort_unstable();
        for row in rows {
            if row >= edit.rows.len() {
                continue;
            }
            let orig = &edit.rows[row];
            let wheres: Vec<String> = pk_idx
                .iter()
                .map(|&i| {
                    let meta = &edit.columns[i];
                    format!(
                        "{} = {}",
                        quote_ident(&meta.name, engine),
                        cell_literal(engine, meta.data_type.family, &orig[i])
                    )
                })
                .collect();
            out.push(format!(
                "DELETE FROM {from} WHERE {};",
                wheres.join(" AND ")
            ));
        }
    }

    out
}

/// Full SQL dump for `table`: header comment, `DROP TABLE IF EXISTS` +
/// `CREATE TABLE` (designer DDL) and multi-row `INSERT` batches through the
/// import machinery with an identity mapping (every column, in order).
fn dump_text(
    engine: Engine,
    table: &ObjectRef,
    detail: &drivers::TableDetail,
    rows: &[Vec<CellValue>],
) -> Result<String, String> {
    let label = table.path.join(".");
    let mut out = String::new();
    out.push_str(&format!("-- jorgenio dbstudio: respaldo de {label}\n"));
    let schema = designer::DesignerState::dump_schema(
        engine,
        table.connection,
        table.clone(),
        detail.clone(),
    );
    if !schema.contains("CREATE TABLE") {
        return Err(format!("no se pudo generar el esquema de «{label}»"));
    }
    out.push_str(&schema);
    out.push('\n');
    let tuples: Vec<String> = rows
        .iter()
        .map(|row| {
            let values: Vec<String> = (0..detail.columns.len())
                .map(|i| {
                    let cell = row.get(i).unwrap_or(&CellValue::Null);
                    cell_literal(engine, detail.columns[i].data_type.family, cell)
                })
                .collect();
            format!("({})", values.join(", "))
        })
        .collect();
    for stmt in import::insert_all_batches(engine, table, &detail.columns, tuples) {
        out.push_str(&stmt);
        out.push('\n');
    }
    Ok(out)
}

/// Read-only account listing per engine; `None` for engines without users.
fn users_list_sql(engine: Engine) -> Option<String> {
    match engine {
        Engine::Mysql | Engine::MariaDb => Some(
            "SELECT User, Host FROM mysql.user ORDER BY User, Host".to_string(),
        ),
        Engine::Postgres => Some(
            "SELECT rolname, CASE WHEN rolsuper THEN 'super' \
                   WHEN NOT rolcanlogin THEN 'grupo' ELSE 'login' END \
                   FROM pg_roles ORDER BY rolname"
                .to_string(),
        ),
        Engine::SqlServer => Some(
            "SELECT name, type_desc FROM sys.database_principals \
                   WHERE type IN ('S','U','G') \
                   AND name NOT IN ('dbo','guest','sys','INFORMATION_SCHEMA') \
                   ORDER BY name"
                .to_string(),
        ),
        Engine::Sqlite | Engine::Mongodb | Engine::Oracle => None,
    }
}

/// Parse a listing result: column 0 is the account, column 1 the host
/// (MySQL) or the class/type detail (others). Short rows yield empty fields
/// instead of panicking (defensive against dialect quirks).
fn users_from_rows(engine: Engine, rows: &[Vec<CellValue>]) -> Vec<UserRow> {
    rows.iter()
        .map(|r| {
            let get =
                |i: usize| r.get(i).map(|c| c.as_display().into_owned()).unwrap_or_default();
            let name = get(0);
            let second = get(1);
            let host = (engine == Engine::Mysql || engine == Engine::MariaDb)
                .then(|| second.clone());
            UserRow {
                name,
                host,
                detail: second,
            }
        })
        .collect()
}

/// CREATE/ALTER statement for the form (preview-ready) or the reason it
/// cannot be generated. MySQL carries user@host identity and an explicit
/// ACCOUNT LOCK/UNLOCK; Postgres uses LOGIN/SUPERUSER flags; password is
/// only appended when non-empty.
fn user_ddl(engine: Engine, action: UserAction, e: &UserEdit) -> Result<String, String> {
    let name = e.name.trim();
    if name.is_empty() {
        return Err("falta el nombre de usuario".to_string());
    }
    match engine {
        Engine::Mysql | Engine::MariaDb => {
            let host_raw = e.host.trim();
            let host = if host_raw.is_empty() { "%" } else { host_raw };
            let ident = format!(
                "{}@{}",
                text_literal(engine, name),
                text_literal(engine, host)
            );
            let mut sql = match action {
                UserAction::Create => format!("CREATE USER {ident}"),
                UserAction::Alter => format!("ALTER USER {ident}"),
            };
            if !e.password.is_empty() {
                sql.push_str(&format!(
                    " IDENTIFIED BY {}",
                    text_literal(engine, &e.password)
                ));
            }
            if action == UserAction::Alter {
                // Always explicit: an "alter" with untouched fields must not
                // silently re-open a locked account — it states its intent.
                sql.push_str(if e.can_login {
                    " ACCOUNT UNLOCK"
                } else {
                    " ACCOUNT LOCK"
                });
            } else if !e.can_login {
                sql.push_str(" ACCOUNT LOCK");
            }
            sql.push(';');
            Ok(sql)
        }
        Engine::Postgres => {
            let ident = quote_ident(name, engine);
            let mut sql = match action {
                UserAction::Create => format!("CREATE USER {ident} WITH"),
                UserAction::Alter => format!("ALTER USER {ident} WITH"),
            };
            sql.push_str(if e.can_login { " LOGIN" } else { " NOLOGIN" });
            sql.push_str(if e.superuser {
                " SUPERUSER"
            } else {
                " NOSUPERUSER"
            });
            if !e.password.is_empty() {
                sql.push_str(&format!(
                    " PASSWORD {}",
                    text_literal(engine, &e.password)
                ));
            }
            sql.push(';');
            Ok(sql)
        }
        Engine::Sqlite | Engine::SqlServer | Engine::Mongodb | Engine::Oracle => Err(
            "gestión de usuarios no disponible para este motor (v1: MySQL y PostgreSQL)"
                .to_string(),
        ),
    }
}

/// DROP for a listed account (MySQL needs the host half of the identity).
fn user_drop_sql(engine: Engine, row: &UserRow) -> Result<String, String> {
    if row.name.trim().is_empty() {
        return Err("fila sin nombre".to_string());
    }
    match engine {
        Engine::Mysql | Engine::MariaDb => {
            let host = row
                .host
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .unwrap_or("%");
            Ok(format!(
                "DROP USER {}@{};",
                text_literal(engine, row.name.trim()),
                text_literal(engine, host)
            ))
        }
        Engine::Postgres => Ok(format!(
            "DROP ROLE IF EXISTS {};",
            quote_ident(row.name.trim(), engine)
        )),
        _ => Err(
            "eliminación no disponible para este motor (v1: MySQL y PostgreSQL)"
                .to_string(),
        ),
    }
}

/// One labeled field of the data-edit form: name (+PK dot, dirty bullet)
/// over a value input and a NULL toggle. `disabled` locks blobs (they
/// cannot round-trip through the text form).
fn field_block<F, G>(
    name: &str,
    pk: bool,
    dirty: bool,
    disabled: bool,
    draft: FieldDraft,
    on_input: F,
    on_null: G,
) -> Element<'static, Message>
where
    F: Fn(String) -> Message + 'static,
    G: Fn(bool) -> Message + 'static,
{
    let mut label = if pk {
        format!("{name} ● PK")
    } else {
        name.to_string()
    };
    if dirty {
        label = format!("• {label}");
    }
    let mut input = text_input(if disabled { "blob (solo lectura)" } else { "valor" }, &draft.text)
        .size(12)
        .padding([4.0, 6.0])
        .width(Length::Fixed(160.0));
    let mut toggle = checkbox(draft.null).size(13);
    if !disabled {
        input = input.on_input(on_input);
        toggle = toggle.on_toggle(on_null);
    }
    container(
        Column::new()
            .push(text(label).size(10).color(theme::muted()))
            .push(
                row![input, toggle, text("NULL").size(10).color(theme::muted())]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
            )
            .spacing(2),
    )
    .padding([4.0, 6.0])
    .style(theme::workspace)
    .into()
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
                        icons::Icon::User,
                        "Usuarios…",
                        ContextAct::Users,
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
                        body = body.push(menu_item(
                            icons::Icon::Import,
                            "Importar datos…",
                            ContextAct::ImportData,
                        ));
                        body = body.push(menu_item(
                            icons::Icon::Save,
                            "Respaldar SQL…",
                            ContextAct::DumpData,
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

    #[test]
    fn objects_panel_filters_rows_and_double_click_uses_time() {
        use std::time::{Duration, Instant};

        let id = ConnectionId::nil();
        let root = ObjectRef::root(id);
        let mut tree: HashMap<ObjectRef, Vec<ObjectNode>> = HashMap::new();
        tree.insert(
            root.clone(),
            vec![
                ObjectNode::leaf(root.child("tienda"), ObjectKind::Database, "tienda"),
                ObjectNode::leaf(root.child("reportes"), ObjectKind::Database, "reportes"),
            ],
        );

        // Empty filter lists everything; a substring narrows; no matches
        // and unknown containers list nothing.
        assert_eq!(filtered_nodes(&tree, &root, "").len(), 2);
        assert_eq!(filtered_nodes(&tree, &root, "tiend").len(), 1);
        assert!(filtered_nodes(&tree, &root, "zzz").is_empty());
        assert!(filtered_nodes(&tree, &root.child("otra"), "").is_empty());

        // Double click: same row, quick succession (<400ms).
        let t0 = Instant::now();
        let last: Option<(usize, Instant)> = Some((0, t0));
        assert!(!is_double_click(None, &0, t0));
        assert!(!is_double_click(Some(&(1, t0)), &0, t0));
        assert!(is_double_click(
            last.as_ref(),
            &0,
            t0 + Duration::from_millis(120)
        ));
        assert!(!is_double_click(
            last.as_ref(),
            &0,
            t0 + Duration::from_millis(401)
        ));
    }

    // ── m16: data grid editing ───────────────────────────────────────────

    /// ColumnMeta shortcut for the data-edit tests.
    fn dcol(name: &str, family: TypeFamily) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            data_type: dbstudio_core::DataType::new(name, family),
            nullable: true,
            comment: None,
        }
    }

    /// Ready-to-edit state over `demo.db.pedidos` with two fetched rows.
    fn edit_fixture() -> DataEditState {
        DataEditState {
            source: obj(&["demo.db", "pedidos"]),
            connection: ConnectionId::nil(),
            sql: "SELECT * FROM \"pedidos\" LIMIT 100;".into(),
            pk: vec!["id".into()],
            loading_pk: false,
            columns: vec![
                dcol("id", TypeFamily::Int),
                dcol("total", TypeFamily::Float),
                dcol("estado", TypeFamily::Text),
            ],
            rows: vec![
                vec![
                    CellValue::Int(7),
                    CellValue::Float(9.5),
                    CellValue::Text("ok".into()),
                ],
                vec![
                    CellValue::Int(8),
                    CellValue::Float(1.0),
                    CellValue::Text("pend".into()),
                ],
            ],
            selected: Some(0),
            insert: None,
            confirm_delete: false,
            drafts: HashMap::new(),
            deletes: HashSet::new(),
        }
    }

    #[test]
    fn text_literal_escapes_per_engine() {
        // MySQL treats backslash as an escape char: it must be doubled too.
        assert_eq!(
            text_literal(Engine::Mysql, "a\\b'c"),
            "'a\\\\b''c'".to_string()
        );
        // Other engines only double the quote.
        assert_eq!(
            text_literal(Engine::Postgres, "a\\b'c"),
            "'a\\b''c'".to_string()
        );
        assert_eq!(
            text_literal(Engine::Sqlite, "o'neil"),
            "'o''neil'".to_string()
        );
    }

    #[test]
    fn draft_literal_toggles_null_and_numbers() {
        // NULL toggle wins over typed text.
        assert_eq!(
            draft_literal(
                Engine::Postgres,
                TypeFamily::Text,
                &FieldDraft {
                    text: "x".into(),
                    null: true
                }
            ),
            "NULL"
        );
        // Numeric families stay unquoted when they parse.
        assert_eq!(
            draft_literal(
                Engine::Postgres,
                TypeFamily::Int,
                &FieldDraft {
                    text: "42".into(),
                    null: false
                }
            ),
            "42"
        );
        // Unparseable numeric falls back to a quoted literal.
        assert_eq!(
            draft_literal(
                Engine::Mysql,
                TypeFamily::Float,
                &FieldDraft {
                    text: "abc".into(),
                    null: false
                }
            ),
            "'abc'"
        );
        // Booleans: TRUE/FALSE on Postgres, 1/0 elsewhere.
        let yes = FieldDraft {
            text: "true".into(),
            null: false,
        };
        assert_eq!(draft_literal(Engine::Postgres, TypeFamily::Bool, &yes), "TRUE");
        assert_eq!(draft_literal(Engine::Mysql, TypeFamily::Bool, &yes), "1");
        let no = FieldDraft {
            text: "0".into(),
            null: false,
        };
        assert_eq!(draft_literal(Engine::Postgres, TypeFamily::Bool, &no), "FALSE");
    }

    #[test]
    fn field_draft_roundtrip_and_dirty_detection() {
        // Text cell → draft → typed-over → dirty; typed back → clean.
        let cell = CellValue::Text("pagada".into());
        assert!(FieldDraft::from_cell(&cell).matches_cell(&cell));
        let typed = FieldDraft {
            text: "fallida".into(),
            null: false,
        };
        assert!(!typed.matches_cell(&cell));
        assert!(typed.matches_cell(&CellValue::Text("fallida".into())));
        // NULL cell roundtrips through the toggle, not the text.
        let nul = FieldDraft::from_cell(&CellValue::Null);
        assert!(nul.null);
        assert!(nul.matches_cell(&CellValue::Null));
        assert!(!FieldDraft {
            text: String::new(),
            null: false
        }
        .matches_cell(&CellValue::Null));
    }

    #[test]
    fn edit_statements_update_sets_dirty_columns_only_with_original_pk() {
        let mut e = edit_fixture();
        // Dirty estado on row 0; total typed back to its original → clean.
        e.drafts.insert(
            (0, 2),
            FieldDraft {
                text: "pagada".into(),
                null: false,
            },
        );
        e.drafts.insert(
            (0, 1),
            FieldDraft {
                text: "9.5".into(),
                null: false,
            },
        );
        let stmts = edit_statements(Engine::Sqlite, &e);
        assert_eq!(
            stmts,
            vec![
                "UPDATE \"pedidos\" SET \"estado\" = 'pagada' WHERE \"id\" = 7;".to_string()
            ]
        );

        // PK clause uses the ORIGINAL value even when the PK itself is edited.
        e.drafts.insert(
            (1, 0),
            FieldDraft {
                text: "99".into(),
                null: false,
            },
        );
        e.selected = Some(1);
        let stmts = edit_statements(Engine::Postgres, &e);
        assert!(stmts.iter().any(|s| s
            == "UPDATE \"pedidos\" SET \"id\" = 99 WHERE \"id\" = 8;"));

        // Rows marked for deletion skip their UPDATEs.
        e.deletes.insert(1);
        let stmts = edit_statements(Engine::Postgres, &e);
        assert!(!stmts.iter().any(|s| s.contains("\"id\" = 99")));
    }

    #[test]
    fn edit_statements_insert_omits_blank_columns_and_defaults() {
        let mut e = edit_fixture();
        // No insert form → no INSERT.
        assert!(edit_statements(Engine::Sqlite, &e).is_empty());

        // Form open but every column blank → DEFAULT VALUES.
        e.insert = Some(HashMap::new());
        let stmts = edit_statements(Engine::Sqlite, &e);
        assert_eq!(
            stmts,
            vec!["INSERT INTO \"pedidos\" DEFAULT VALUES;".to_string()]
        );
        // MySQL spells the empty row differently (path = [db, table]).
        e.source = obj(&["demo", "pedidos"]);
        assert_eq!(
            edit_statements(Engine::Mysql, &e),
            vec!["INSERT INTO `demo`.`pedidos` () VALUES ();".to_string()]
        );
        e.source = obj(&["demo.db", "pedidos"]);

        // One filled column → only that column appears.
        let mut ins = HashMap::new();
        ins.insert(
            2,
            FieldDraft {
                text: "nueva".into(),
                null: false,
            },
        );
        ins.insert(
            1,
            FieldDraft {
                text: "   ".into(),
                null: false,
            },
        );
        e.insert = Some(ins);
        let stmts = edit_statements(Engine::Sqlite, &e);
        assert_eq!(
            stmts,
            vec![
                "INSERT INTO \"pedidos\" (\"estado\") VALUES ('nueva');".to_string()
            ]
        );

        // NULL toggle counts as filled and emits NULL.
        let mut ins = HashMap::new();
        ins.insert(
            1,
            FieldDraft {
                text: String::new(),
                null: true,
            },
        );
        e.insert = Some(ins);
        let stmts = edit_statements(Engine::Postgres, &e);
        assert_eq!(
            stmts,
            vec![
                "INSERT INTO \"pedidos\" (\"total\") VALUES (NULL);".to_string()
            ]
        );
    }

    #[test]
    fn edit_statements_delete_by_pk_in_stable_order() {
        let mut e = edit_fixture();
        e.deletes.insert(1);
        e.deletes.insert(0);
        let stmts = edit_statements(Engine::Sqlite, &e);
        assert_eq!(
            stmts,
            vec![
                "DELETE FROM \"pedidos\" WHERE \"id\" = 7;".to_string(),
                "DELETE FROM \"pedidos\" WHERE \"id\" = 8;".to_string(),
            ]
        );
        // A table without a resolvable PK yields nothing (install-time guard).
        e.pk = vec!["no_existe".into()];
        assert!(edit_statements(Engine::Sqlite, &e).is_empty());
    }

    #[test]
    fn import_state_maps_by_name_and_flags_duplicates() {
        let mut st = ImportState::new(obj(&["demo.db", "pedidos"]), ConnectionId::nil());
        st.file_columns = vec![" id ".into(), "NOMBRE".into(), "extra".into()];
        st.columns = vec![
            dcol("id", TypeFamily::Int),
            dcol("nombre", TypeFamily::Text),
        ];
        st.loading = false;
        st.remap();
        assert_eq!(st.mapped_count(), 2);
        assert_eq!(st.mapping, vec![Some(0), Some(1), None]);
        assert!(st.duplicate_target().is_none());
        // Two file columns pointed at the same target → flagged for Run.
        st.mapping = vec![Some(0), Some(0), None];
        assert!(
            st.duplicate_target()
                .is_some_and(|d| d.contains("dos veces"))
        );
        // Empty mapping blocks the run button (mapped_count == 0).
        st.mapping = vec![None, None, None];
        assert_eq!(st.mapped_count(), 0);
    }

    #[test]
    fn auto_refreshable_accepts_single_readonly_statements() {
        assert!(auto_refreshable("SELECT * FROM usuarios"));
        assert!(auto_refreshable("select 1"));
        assert!(auto_refreshable(
            "WITH t AS (SELECT 1) SELECT * FROM t;"
        ));
        assert!(auto_refreshable("SHOW TABLES"));
        assert!(auto_refreshable("PRAGMA table_info(usuarios)"));
        assert!(auto_refreshable("EXPLAIN SELECT id FROM usuarios"));
        // Trailing semicolon + whitespace is still a single statement.
        assert!(auto_refreshable("SELECT id FROM usuarios;  \n"));
    }

    #[test]
    fn auto_refreshable_rejects_writes_multistatement_and_empty() {
        assert!(!auto_refreshable("INSERT INTO usuarios (id) VALUES (1)"));
        assert!(!auto_refreshable("DELETE FROM usuarios"));
        assert!(!auto_refreshable("UPDATE usuarios SET id = 1"));
        assert!(!auto_refreshable("DROP TABLE usuarios"));
        assert!(!auto_refreshable("TRUNCATE usuarios"));
        assert!(!auto_refreshable(""));
        assert!(!auto_refreshable("   \n\t"));
        // Two statements, even a benign prefix with a trailing write.
        assert!(!auto_refreshable("SELECT 1; DROP TABLE usuarios"));
        // Data-modifying CTE behind a WITH opener.
        assert!(!auto_refreshable(
            "WITH x AS (DELETE FROM usuarios RETURNING *) SELECT 1"
        ));
        // Write keyword hidden in a comment still trips the word scan.
        assert!(!auto_refreshable("-- DELETE\nSELECT 1"));
        // The first keyword decides (a leading comment makes it unknown).
        assert!(!auto_refreshable("-- hola\nSELECT 1"));
    }

    #[test]
    fn refresh_every_periods_and_labels() {
        use std::time::Duration;
        assert_eq!(RefreshEvery::Off.period(), None);
        assert_eq!(
            RefreshEvery::Sec1.period(),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            RefreshEvery::Sec30.period(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(RefreshEvery::Off.to_string(), "no");
        assert_eq!(RefreshEvery::Sec10.to_string(), "10 s");
        assert_eq!(RefreshEvery::ALL.len(), 5);
        assert_eq!(RefreshEvery::ALL[0], RefreshEvery::Off);
    }

    #[test]
    fn dump_text_emits_drop_create_and_batched_inserts() {
        let table = obj(&["demo.db", "pedidos"]);
        let detail = drivers::TableDetail {
            columns: vec![
                dcol("id", TypeFamily::Int),
                dcol("nombre", TypeFamily::Text),
            ],
            primary_keys: vec!["id".into()],
            foreign_keys: Vec::new(),
        };
        let rows = vec![
            vec![CellValue::Int(1), CellValue::Text("ana".into())],
            vec![CellValue::Int(2), CellValue::Null],
        ];
        let text = dump_text(Engine::Sqlite, &table, &detail, &rows).expect("dump");
        assert!(text.starts_with("-- jorgenio dbstudio: respaldo de demo.db.pedidos"));
        assert!(text.contains("DROP TABLE IF EXISTS"));
        assert!(text.contains("CREATE TABLE"));
        assert!(text.contains("PRIMARY KEY"));
        // Identity mapping: every column, in order, values literal-typed
        // (numbers unquoted, text quoted, NULL for blank non-text).
        assert!(text.contains("INSERT INTO"));
        assert!(text.contains("(1, 'ana'), (2, NULL)"));
        assert!(!text.contains("'1'"));
        // No rows: schema-only script.
        let empty = dump_text(Engine::Sqlite, &table, &detail, &[]).expect("schema");
        assert!(empty.contains("CREATE TABLE"));
        assert!(!empty.contains("INSERT INTO"));
    }

    #[test]
    fn users_list_sql_covers_sql_engines_only() {
        assert!(users_list_sql(Engine::Mysql).is_some_and(|s| s.contains("mysql.user")));
        assert!(users_list_sql(Engine::MariaDb).is_some_and(|s| s.contains("mysql.user")));
        assert!(users_list_sql(Engine::Postgres).is_some_and(|s| s.contains("pg_roles")));
        assert!(
            users_list_sql(Engine::SqlServer)
                .is_some_and(|s| s.contains("database_principals"))
        );
        assert!(users_list_sql(Engine::Sqlite).is_none());
        assert!(users_list_sql(Engine::Mongodb).is_none());
        assert!(users_list_sql(Engine::Oracle).is_none());
    }

    #[test]
    fn user_ddl_mysql_create_alter_and_drop() {
        let mut e = UserEdit::create();
        e.name = "ana".into();
        e.host = "%".into();
        e.password = "secreto".into();
        assert_eq!(
            user_ddl(Engine::Mysql, UserAction::Create, &e).expect("ddl"),
            "CREATE USER 'ana'@'%' IDENTIFIED BY 'secreto';"
        );
        // Host en blanco → % por defecto; comilla escapada al estilo MySQL.
        e.host = "  ".into();
        e.password = "p'1".into();
        let sql = user_ddl(Engine::Mysql, UserAction::Create, &e).expect("ddl");
        assert!(sql.starts_with("CREATE USER 'ana'@'%' IDENTIFIED BY "));
        assert!(sql.contains("''"));
        // Sin contraseña: sin cláusula IDENTIFIED BY.
        e.password.clear();
        assert_eq!(
            user_ddl(Engine::Mysql, UserAction::Create, &e).expect("ddl"),
            "CREATE USER 'ana'@'%';"
        );
        // ALTER always states an explicit lock clause (can re-open a lock).
        assert_eq!(
            user_ddl(Engine::Mysql, UserAction::Alter, &e).expect("ddl"),
            "ALTER USER 'ana'@'%' ACCOUNT UNLOCK;"
        );
        e.can_login = false;
        assert_eq!(
            user_ddl(Engine::Mysql, UserAction::Alter, &e).expect("ddl"),
            "ALTER USER 'ana'@'%' ACCOUNT LOCK;"
        );
        // DROP carries the host half of the identity (default %).
        let row = UserRow {
            name: "ana".into(),
            host: Some("localhost".into()),
            detail: "localhost".into(),
        };
        assert_eq!(
            user_drop_sql(Engine::Mysql, &row).expect("drop"),
            "DROP USER 'ana'@'localhost';"
        );
        let bare = UserRow {
            name: "ana".into(),
            host: None,
            detail: String::new(),
        };
        assert_eq!(
            user_drop_sql(Engine::Mysql, &bare).expect("drop"),
            "DROP USER 'ana'@'%';"
        );
    }

    #[test]
    fn user_ddl_postgres_flags_and_password() {
        let mut e = UserEdit::create();
        e.name = "ana".into();
        e.password = "x y".into();
        assert_eq!(
            user_ddl(Engine::Postgres, UserAction::Create, &e).expect("ddl"),
            "CREATE USER \"ana\" WITH LOGIN NOSUPERUSER PASSWORD 'x y';"
        );
        e.name = "admin".into();
        e.can_login = false;
        e.superuser = true;
        assert_eq!(
            user_ddl(Engine::Postgres, UserAction::Create, &e).expect("ddl"),
            "CREATE USER \"admin\" WITH NOLOGIN SUPERUSER PASSWORD 'x y';"
        );
        let sql = user_ddl(Engine::Postgres, UserAction::Alter, &e).expect("ddl");
        assert!(sql.starts_with("ALTER USER \"admin\" WITH NOLOGIN SUPERUSER"));
        let row = UserRow {
            name: "admin".into(),
            host: None,
            detail: "login".into(),
        };
        assert_eq!(
            user_drop_sql(Engine::Postgres, &row).expect("drop"),
            "DROP ROLE IF EXISTS \"admin\";"
        );
    }

    #[test]
    fn user_ddl_rejects_empty_names_and_unsupported_engines() {
        let empty = UserEdit::create();
        assert!(user_ddl(Engine::Postgres, UserAction::Create, &empty).is_err());
        let mut named = UserEdit::create();
        named.name = "ana".into();
        assert!(user_ddl(Engine::SqlServer, UserAction::Create, &named).is_err());
        let row = UserRow {
            name: "ana".into(),
            host: None,
            detail: String::new(),
        };
        assert!(user_drop_sql(Engine::Sqlite, &row).is_err());
        assert!(user_drop_sql(Engine::SqlServer, &row).is_err());
    }

    #[test]
    fn users_from_rows_parses_name_host_and_detail() {
        let rows = vec![
            vec![
                CellValue::Text("ana".into()),
                CellValue::Text("localhost".into()),
            ],
            vec![CellValue::Text("luis".into()), CellValue::Text("%".into())],
        ];
        let mine = users_from_rows(Engine::Mysql, &rows);
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].name, "ana");
        assert_eq!(mine[0].host.as_deref(), Some("localhost"));
        assert_eq!(mine[0].detail, "localhost");
        let pg = users_from_rows(Engine::Postgres, &rows);
        assert_eq!(pg[0].host, None);
        assert_eq!(pg[0].detail, "localhost");
        // Short row: no panic, empty fields.
        let short = users_from_rows(Engine::Mysql, &[vec![CellValue::Text("x".into())]]);
        assert_eq!(short[0].name, "x");
        assert_eq!(short[0].host.as_deref(), Some(""));
    }

    #[test]
    fn user_edit_prefills_alter_from_listing() {
        let row = UserRow {
            name: "carla".into(),
            host: None,
            detail: "grupo".into(),
        };
        let edit = UserEdit::alter(&row);
        assert_eq!(edit.action(), UserAction::Alter);
        assert_eq!(edit.altered.as_deref(), Some("carla"));
        assert!(!edit.can_login);
        assert!(!edit.superuser);
        let super_row = UserRow {
            name: "admin".into(),
            host: None,
            detail: "super".into(),
        };
        let su = UserEdit::alter(&super_row);
        assert!(su.can_login);
        assert!(su.superuser);
    }

    #[test]
    fn history_push_dedupes_moves_to_front_and_caps() {
        let mut entries = Vec::new();
        history_push(&mut entries, "   ".into(), None);
        assert!(entries.is_empty(), "blank statements never record");

        history_push(&mut entries, "SELECT 1".into(), Some("demo".into()));
        history_push(&mut entries, "SELECT 2".into(), None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sql, "SELECT 2");

        // Repeat → moves to the head with the fresh connection, no dup.
        history_push(&mut entries, "SELECT 1".into(), Some("prod".into()));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sql, "SELECT 1");
        assert_eq!(entries[0].conn.as_deref(), Some("prod"));

        // Cap: the oldest statements fall off the tail.
        for i in 0..(store::HISTORY_CAP + 10) {
            history_push(&mut entries, format!("SELECT {i}"), None);
        }
        assert_eq!(entries.len(), store::HISTORY_CAP);
        assert_eq!(
            entries[0].sql,
            format!("SELECT {}", store::HISTORY_CAP + 9)
        );
    }

    #[test]
    fn history_line_collapses_whitespace_and_truncates() {
        assert_eq!(history_line("  SELECT  *\n FROM t "), "SELECT * FROM t");
        let long = history_line(&"SELECT ".repeat(60));
        assert!(long.chars().count() <= 121, "preview must stay one row");
        assert!(long.ends_with('…'));
    }
}


