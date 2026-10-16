//! DBStudio: three-pane desktop client. The navigator holds saved
//! connections plus a lazy object tree backed by a live driver session; the
//! editor runs statements through `dbstudio-drivers` and the result grid is
//! rebuilt from the driver's normalized `QueryResult`.

mod export;
mod form;
mod grid;
mod store;
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
use iced::widget::{button, container, row, rule, scrollable, stack, text, Column, MouseArea, Space};
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
    session: Option<Arc<Session>>,
    active_id: Option<ConnectionId>,
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
    Connected(Result<ConnectedPayload, String>),
    ChildrenLoaded(ObjectRef, Result<Vec<ObjectNode>, String>),
    TreeToggle(ObjectRef),
    /// Table/view/collection clicked: put a starter query in the editor and
    /// expand the node's children.
    TreeOpenObject(ObjectRef, ObjectKind),
    Executed(Result<QueryResult, String>),
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
}

type ConnectedPayload = (ConnectionId, Arc<Session>, ObjectRef, Vec<ObjectNode>);

fn boot() -> (App, Task<Message>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Derive the editor palette from the app theme so the code pane matches
    // the light chrome, then lift the surfaces to pure white.
    let mut editor = CodeEditor::new(SAMPLE_SQL, "sql");
    let mut editor_style = iced_code_editor::from_iced_theme(&theme::app_theme());
    editor_style.background = iced::color!(0xffffff);
    editor_style.gutter_background = iced::color!(0xfafbfd);
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
            session: None,
            active_id: None,
            root: None,
            tree: HashMap::new(),
            expanded: HashSet::new(),
            last_columns: Vec::new(),
            last_row_count: 0,
            executed: false,
            query_context: None,
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
            let res = rt
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
                .map_err(|e| e.to_string())?;
            res.map(|(session, children)| (id, session, root_reply, children))
        },
        Message::Connected,
    )
}

fn execute_task(
    rt: Arc<tokio::runtime::Runtime>,
    client: std::sync::Arc<dyn drivers::DatabaseClient>,
    req: ExecuteRequest,
) -> Task<Message> {
    Task::perform(
        async move {
            rt.spawn(async move { client.execute(req).await.map_err(|e| e.to_string()) })
                .await
                .map_err(|e| e.to_string())?
        },
        Message::Executed,
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

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Editor(msg) => {
                let _ = self.editor.update(&msg);
                Task::none()
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
            Message::SelectConnection(i) => {
                self.selected = Some(i);
                let (name, state) = {
                    let e = &self.entries[i];
                    (e.cfg.name.clone(), e.state.clone())
                };
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
            Message::DeleteSelected => {
                let Some(i) = self.selected.take() else {
                    return Task::none();
                };
                if i >= self.entries.len() {
                    return Task::none();
                }
                let removed = self.entries.remove(i);
                if self.active_id == Some(removed.cfg.id) {
                    // Dropping the session closes its tunnel/client.
                    self.session = None;
                    self.active_id = None;
                    self.root = None;
                    self.tree.clear();
                    self.expanded.clear();
                    self.query_context = None;
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
                let Some(session) = &self.session else {
                    self.status =
                        "sin conexión — abre «+ Nueva conexión» primero".to_string();
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
                execute_task(self.rt.clone(), client, req)
            }
            Message::OpenForm => {
                self.form = Some(ConnForm::default());
                Task::none()
            }
            Message::Form(FormMsg::Cancel) => {
                self.form = None;
                Task::none()
            }
            Message::Form(FormMsg::Submit) => {
                let Some(form) = self.form.take() else {
                    return Task::none();
                };
                match form.build() {
                    Ok(cfg) => {
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
                self.tree.insert(root.clone(), children);
                self.expanded.clear();
                self.expanded.insert(root.clone());
                self.query_context = None;
                self.root = Some(root);
                self.session = Some(session);
                self.active_id = Some(id);
                self.status = match self.entries.iter().find(|e| e.cfg.id == id) {
                    Some(e) => format!("conectada: {} ({})", e.cfg.name, e.cfg.engine),
                    None => "conectada".to_string(),
                };
                Task::none()
            }
            Message::Connected(Err(e)) => {
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|e| matches!(e.state, ConnState::Connecting))
                {
                    entry.state = ConnState::Error(e.clone());
                }
                if self.active_id.is_none() {
                    self.session = None;
                }
                self.status = format!("error de conexión: {e}");
                Task::none()
            }
            Message::TreeToggle(node) => self.expand(&node),
            Message::TreeOpenObject(obj, kind) => {
                let sql = self.query_for(&obj, kind);
                self.query_context = match kind {
                    ObjectKind::Collection => Some(obj.path.join(".")),
                    _ => None,
                };
                let expand = self.expand(&obj);
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
            Message::Executed(Ok(result)) => {
                let ms = result.elapsed.as_millis();
                self.executed = true;
                self.status = if result.is_rows() {
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
                self.last_columns = result.columns.clone();
                self.last_row_count = result.row_count();
                let (grid, init) = grid::build(result);
                self.grid = grid;
                init.map(Message::Grid)
            }
            Message::Executed(Err(e)) => {
                self.status = format!("error: {e}");
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
        let Some(session) = &self.session else {
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

        let toolbar = row![
            text("DBStudio").size(14),
            button(text("+ Nueva conexión").size(13))
                .on_press(Message::OpenForm)
                .style(theme::flat),
            button(text("Ejecutar (F5)").size(13))
                .on_press(Message::Run)
                .style(theme::solid),
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
                .height(Length::Fixed(40.0))
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
                    button(text("✕ Eliminar").size(11))
                        .on_press(Message::DeleteSelected)
                        .style(theme::flat),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::End),
            );
        }
        navigator = navigator.push(conn_header);
        navigator = navigator.push(
            button(text("+ Nueva conexión").size(13))
                .width(Length::Fill)
                .on_press(Message::OpenForm)
                .style(theme::flat),
        );
        if let Some(form) = &self.form {
            navigator = navigator.push(
                container(form::view(form).map(Message::Form))
                    .padding(6)
                    .style(theme::workspace),
            );
        } else {
            for (i, entry) in self.entries.iter().enumerate() {
                let (marker, marker_color) = match &entry.state {
                    ConnState::Connected => ("●", theme::engine_color(entry.cfg.engine)),
                    ConnState::Connecting => ("◐", theme::engine_color(entry.cfg.engine)),
                    ConnState::Idle => ("○", theme::muted()),
                    ConnState::Error(_) => ("✕", iced::color!(0xd93025)),
                };
                let label = row![
                    text(marker).size(11).color(marker_color),
                    text(entry.cfg.name.clone()).size(13),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center);
                let row_style: fn(&Theme, button::Status) -> button::Style =
                    if self.selected == Some(i) {
                        theme::row_selected
                    } else {
                        theme::flat
                    };
                navigator = navigator.push(
                    button(label)
                        .width(Length::Fill)
                        .on_press(Message::SelectConnection(i))
                        .style(row_style),
                );
            }
            if let Some(root) = &self.root {
                navigator = push_tree(self, root, 0, navigator);
            }
        }
        let navigator = container(scrollable(navigator).height(Length::Fill))
            .width(Length::Fixed(260.0))
            .height(Length::Fill)
            .padding(6)
            .style(theme::navigator);

        // ── editor pane with its tab strip ──────────────────────────────
        let editor_tabs = container(
            row![button(text("  Consulta 1  ").size(13)).style(theme::tab_active)]
                .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding(iced::Padding { top: 6.0, right: 6.0, bottom: 0.0, left: 6.0 })
        .style(theme::chrome);
        let editor = container(self.editor.view().map(Message::Editor))
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

        let main = row![
            navigator,
                Column::new()
                .push(editor_tabs)
                .push(editor.height(Length::FillPortion(2)))
                .push(rule::horizontal(1).style(theme::hairline))
                .push(result_strip)
                .push(grid.height(Length::FillPortion(3)))
                .width(Length::Fill)
        ];

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
        theme::app_theme()
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

fn push_tree<'a>(
    app: &'a App,
    parent: &ObjectRef,
    depth: usize,
    mut col: Column<'a, Message>,
) -> Column<'a, Message> {
    let Some(children) = app.tree.get(parent) else {
        return col;
    };
    for node in children {
        let is_open = app.expanded.contains(&node.object);
        let marker = match (node.has_children, is_open) {
            (true, true) => "▾",
            (true, false) => "▸",
            (false, _) => "·",
        };
        let pad = "  ".repeat(depth);
        let press = match node.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::Collection => {
                Message::TreeOpenObject(node.object.clone(), node.kind)
            }
            _ => Message::TreeToggle(node.object.clone()),
        };
        col = col.push(
            button(text(format!("{pad}{marker} {}", node.label)).size(13))
                .width(Length::Fill)
                .style(theme::flat)
                .on_press(press),
        );
        if is_open {
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
        .title("DBStudio")
        .theme(App::theme)
        .subscription(App::subscription)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}


