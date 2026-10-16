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
    CellValue, ColumnMeta, ConnectionConfig, ConnectionId, ExecuteRequest, ObjectNode,
    ObjectRef, QueryResult,
};
use dbstudio_drivers::{self as drivers, Session};
use form::{ConnForm, FormMsg};
use store::Connections;
use iced::widget::{button, container, row, rule, scrollable, text, Column};
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
    status: String,
}

#[derive(Clone)]
enum Message {
    Editor(EditorMessage),
    Grid(grid::GridEvent),
    SearchInput(String),
    SearchSubmit(String),
    SearchSelect(String),
    SelectConnection(usize),
    Run,
    OpenForm,
    Form(FormMsg),
    Connected(Result<ConnectedPayload, String>),
    ChildrenLoaded(ObjectRef, Result<Vec<ObjectNode>, String>),
    TreeToggle(ObjectRef),
    Executed(Result<QueryResult, String>),
    /// Copy selected rows (all when nothing is selected) as TSV.
    CopyValues,
    /// Copy rows as TSV with a header line of column names.
    CopyWithHeaders,
    /// Open the save dialog for the current result set.
    Export,
    /// The save dialog resolved (None = cancelled).
    ExportTo(Option<std::path::PathBuf>),
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
            status,
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
                execute_task(
                    self.rt.clone(),
                    client,
                    ExecuteRequest::new(sql).with_max_rows(MAX_ROWS),
                )
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
                if let Some(e) = self.entries.iter_mut().find(|e| e.cfg.id == id) {
                    e.state = ConnState::Connected;
                }
                self.tree.insert(root.clone(), children);
                self.expanded.clear();
                self.expanded.insert(root.clone());
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
            Message::TreeToggle(node) => {
                if self.expanded.contains(&node) {
                    self.expanded.remove(&node);
                    return Task::none();
                }
                self.expanded.insert(node.clone());
                if self.tree.contains_key(&node) {
                    return Task::none();
                }
                let Some(session) = &self.session else {
                    return Task::none();
                };
                let client = session.client.clone();
                let rt = self.rt.clone();
                let reply = node.clone();
                Task::perform(
                    async move {
                        rt.spawn(async move {
                            client.list_children(&node).await.map_err(|e| e.to_string())
                        })
                        .await
                        .map_err(|e| e.to_string())?
                    },
                    move |res| Message::ChildrenLoaded(reply.clone(), res),
                )
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

    fn view(&self) -> Element<'_, Message> {
        // ── toolbar ─────────────────────────────────────────────────────
        let toolbar = container(
            row![
                button(text("Nueva conexión").size(13))
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
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .padding(8),
        )
        .width(Length::Fill)
        .style(theme::chrome);

        // ── navigator: connections + lazy object tree ───────────────────
        let mut navigator: Column<'_, Message> = Column::new().spacing(2);
        navigator = navigator.push(
            text("CONEXIONES").size(11).color(theme::muted()),
        );
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
                navigator = navigator.push(
                    button(label)
                        .width(Length::Fill)
                        .on_press(Message::SelectConnection(i))
                        .style(theme::flat),
                );
            }
            if let Some(root) = &self.root {
                navigator = push_tree(self, root, 0, navigator);
            }
        }
        let navigator = container(scrollable(navigator).height(Length::Fill).anchor_bottom())
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
        let result_strip = container(
            row![
                button(text("  Resultado 1  ").size(13)).style(theme::tab_active),
                text(format!("{} filas", self.last_row_count))
                    .size(12)
                    .color(theme::muted()),
                container(
                    row![
                        button(text("Copiar").size(12))
                            .on_press(Message::CopyValues)
                            .style(theme::flat),
                        button(text("Copiar c/nombres").size(12))
                            .on_press(Message::CopyWithHeaders)
                            .style(theme::flat),
                        button(text("Exportar…").size(12))
                            .on_press(Message::Export)
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
        let conn_label = self
            .active_id
            .and_then(|id| self.entries.iter().find(|e| e.cfg.id == id))
            .map(|e| format!("● {} · {}", e.cfg.engine, e.cfg.name))
            .unwrap_or_else(|| "sin conexión".to_string());
        let status_bar = container(
            row![
                text(format!(" {}", self.status)).size(12),
                container(
                    row![text(conn_label).size(12).color(theme::muted())]
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

        Column::new()
            .push(toolbar)
            .push(rule::horizontal(1).style(theme::hairline))
            .push(main.height(Length::Fill))
            .push(rule::horizontal(1).style(theme::hairline))
            .push(status_bar)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::keyboard::listen().filter_map(|event| {
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
                _ => None,
            }
        })
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
        col = col.push(
            button(text(format!("{pad}{marker} {}", node.label)).size(13))
                .width(Length::Fill)
                .on_press(Message::TreeToggle(node.object.clone())),
        );
        if is_open {
            col = push_tree(app, &node.object, depth + 1, col);
        }
    }
    col
}

fn main() -> iced::Result {
    iced::application(boot, App::update, App::view)
        .title("DBStudio")
        .theme(App::theme)
        .subscription(App::subscription)
        .run()
}


