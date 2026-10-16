//! New-connection form rendered inside the navigator pane.

use dbstudio_core::{ConnectionConfig, Engine};
use iced::widget::{button, column, pick_list, row, text, text_input};
use iced::{Element, Length};

#[derive(Debug, Clone)]
pub enum FormMsg {
    Name(String),
    Engine(Engine),
    Host(String),
    Port(String),
    Database(String),
    Username(String),
    Password(String),
    Submit,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct ConnForm {
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: String,
    pub database: String,
    pub username: String,
    pub password: String,
}

impl Default for ConnForm {
    fn default() -> Self {
        let engine = Engine::Sqlite;
        Self {
            name: "demo-sqlite".to_string(),
            engine,
            host: format!("{}/../../sample/demo.db", env!("CARGO_MANIFEST_DIR")),
            port: String::new(),
            database: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl ConnForm {
    pub fn update(&mut self, msg: FormMsg) {
        match msg {
            FormMsg::Name(v) => self.name = v,
            FormMsg::Engine(engine) => {
                self.engine = engine;
                self.port = if engine == Engine::Sqlite {
                    String::new()
                } else {
                    engine.default_port().to_string()
                };
            }
            FormMsg::Host(v) => self.host = v,
            FormMsg::Port(v) => self.port = v,
            FormMsg::Database(v) => self.database = v,
            FormMsg::Username(v) => self.username = v,
            FormMsg::Password(v) => self.password = v,
            FormMsg::Submit | FormMsg::Cancel => {}
        }
    }

    /// Validate the form and produce the driver config.
    pub fn build(&self) -> Result<ConnectionConfig, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("el nombre no puede estar vacío".into());
        }
        let mut cfg = ConnectionConfig::new(name, self.engine);
        let host = self.host.trim();
        if host.is_empty() {
            return Err(if self.engine == Engine::Sqlite {
                "indica la ruta del archivo .db".to_string()
            } else {
                "indica el host".to_string()
            });
        }
        cfg = cfg.with_host(host);

        if self.engine == Engine::Sqlite {
            return Ok(cfg);
        }

        let port = if self.port.trim().is_empty() {
            self.engine.default_port()
        } else {
            self.port
                .trim()
                .parse()
                .map_err(|_| format!("puerto inválido: {}", self.port.trim()))?
        };
        cfg = cfg
            .with_port(port)
            .with_database(self.database.trim())
            .with_username(self.username.trim());
        if !self.password.is_empty() {
            cfg = cfg.with_password(self.password.as_str());
        }
        Ok(cfg)
    }
}

pub fn view(form: &ConnForm) -> Element<'_, FormMsg> {
    let sqlite = form.engine == Engine::Sqlite;

    let mut fields = column![
        text("Nueva conexión").size(14),
        labeled("Nombre", text_input("nombre", &form.name).on_input(FormMsg::Name)),
        labeled(
            "Motor",
            pick_list(
                Engine::ALL,
                Some(form.engine),
                FormMsg::Engine
            )
            .width(Length::Fill),
        ),
        labeled(
            if sqlite { "Archivo (.db)" } else { "Host" },
            text_input(if sqlite { "ruta/archivo.db" } else { "host" }, &form.host).on_input(FormMsg::Host),
        ),
    ]
    .spacing(6);

    if !sqlite {
        fields = fields.push(labeled(
            "Puerto",
            text_input("puerto", &form.port).on_input(FormMsg::Port),
        ));
        fields = fields.push(labeled(
            "Base de datos",
            text_input("database", &form.database).on_input(FormMsg::Database),
        ));
        fields = fields.push(labeled(
            "Usuario",
            text_input("usuario", &form.username).on_input(FormMsg::Username),
        ));
        fields = fields.push(labeled(
            "Contraseña",
            text_input("••••••", &form.password).on_input(FormMsg::Password),
        ));
    }

    fields
        .push(
            row![
                button(text("Cancelar")).on_press(FormMsg::Cancel),
                button(text("Conectar")).on_press(FormMsg::Submit),
            ]
            .spacing(8),
        )
        .into()
}

fn labeled<'a>(label: &'a str, control: impl Into<Element<'a, FormMsg>>) -> Element<'a, FormMsg> {
    column![text(label).size(12), control.into()].spacing(2).into()
}

