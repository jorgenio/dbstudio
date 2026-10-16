//! DBStudio: three-pane desktop client. The navigator holds saved
//! connections plus a lazy object tree backed by a live driver session; the
//! editor runs statements through `dbstudio-drivers` and the result grid is
//! rebuilt from the driver's normalized `QueryResult`.

mod form;
mod grid;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbstudio_core::{
    ConnectionConfig, ConnectionId, ExecuteRequest, ObjectNode, ObjectRef, QueryResult,
};
use dbstudio_drivers::{self as drivers, Session};
use form::{ConnForm, FormMsg};
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
}

type ConnectedPayload = (ConnectionId, Arc<Session>, ObjectRef, Vec<ObjectNode>);

fn boot() -> (App, Task<Message>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    (
        App {
            rt: Arc::new(rt),
            editor: CodeEditor::new(SAMPLE_SQL, "sql"),
            grid: grid::empty(),
            search: AutocompleteState::new(Vec::new(), Filter::None),
            search_text: String::new(),
            entries: Vec::new(),
            selected: None,
            form: None,
            session: None,
            active_id: None,
            root: None,
            tree: HashMap::new(),
            expanded: HashSet::new(),
            status: "listo — «+ Nueva conexión» para empezar".to_string(),
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
                let (grid, init) = grid::build(result);
                self.grid = grid;
                init.map(Message::Grid)
            }
            Message::Executed(Err(e)) => {
                self.status = format!("error: {e}");
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let toolbar = row![
            text("DBStudio").size(18),
            container(auto_complete(
                &self.search,
                "Buscar conexión…",
                Message::SearchInput,
                Message::SearchSubmit,
                Some(Message::SearchSelect),
            ))
            .width(Length::Fixed(280.0))
            .padding(4),
            button(text("Ejecutar (F5)")).on_press(Message::Run),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .padding(8);

        let mut navigator: Column<'_, Message> = Column::new().spacing(4);
        navigator = navigator.push(
            button(text("+ Nueva conexión"))
                .width(Length::Fill)
                .on_press(Message::OpenForm),
        );
        if let Some(form) = &self.form {
            navigator = navigator.push(form::view(form).map(Message::Form));
        } else {
            for (i, entry) in self.entries.iter().enumerate() {
                let marker = match &entry.state {
                    ConnState::Connected => "●",
                    ConnState::Connecting => "◐",
                    ConnState::Error(_) => "✕",
                };
                let label = format!("{marker} {}", entry.cfg.name);
                navigator = navigator.push(
                    button(text(label))
                        .width(Length::Fill)
                        .on_press(Message::SelectConnection(i)),
                );
            }
            if let Some(root) = &self.root {
                navigator = push_tree(self, root, 0, navigator);
            }
        }
        let navigator = container(scrollable(navigator).height(Length::Fill).anchor_bottom())
            .width(Length::Fixed(260.0))
            .height(Length::Fill)
            .style(container::secondary);

        let editor = container(self.editor.view().map(Message::Editor))
            .height(Length::FillPortion(2))
            .padding(4);
        let grid = container(self.grid.view().map(Message::Grid))
            .height(Length::FillPortion(3));

        let main = row![
            navigator,
            Column::new()
                .push(editor)
                .push(rule::horizontal(1))
                .push(grid)
                .width(Length::Fill)
        ];

        let status_bar = container(text(format!(" {}", self.status)).size(13))
            .width(Length::Fill)
            .padding(4)
            .style(container::secondary);

        Column::new()
            .push(toolbar)
            .push(rule::horizontal(1))
            .push(main.height(Length::Fill))
            .push(rule::horizontal(1))
            .push(status_bar)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::keyboard::listen().filter_map(|event| match event {
            iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat: false,
                ..
            } if modifiers.is_empty()
                && matches!(
                    key,
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::F5)
                ) =>
            {
                Some(Message::Run)
            }
            _ => None,
        })
    }

    fn theme(&self) -> Theme {
        Theme::Dark
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
