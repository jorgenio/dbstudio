//! New-connection form rendered inside the navigator pane.

use std::fmt;
use std::path::PathBuf;

use dbstudio_core::{
    ConnectionConfig, Engine, HostKeyPolicy, SecretString, SshAuth, SshConfig,
};
use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input};
use iced::{Element, Length};

use crate::theme;

#[derive(Debug, Clone)]
pub enum FormMsg {
    Name(String),
    Engine(Engine),
    Host(String),
    Port(String),
    Database(String),
    Username(String),
    Password(String),
    SshToggled(bool),
    SshHost(String),
    SshPort(String),
    SshUser(String),
    SshAuthKind(SshAuthKind),
    SshPassword(String),
    SshKeyPath(String),
    SshPassphrase(String),
    SshPolicy(PolicyChoice),
    Submit,
    Cancel,
}

/// How the SSH tunnel authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthKind {
    Password,
    Key,
    Agent,
}

impl SshAuthKind {
    pub const ALL: [SshAuthKind; 3] = [SshAuthKind::Password, SshAuthKind::Key, SshAuthKind::Agent];
}

impl fmt::Display for SshAuthKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SshAuthKind::Password => "Contraseña",
            SshAuthKind::Key => "Clave privada",
            SshAuthKind::Agent => "Agente SSH",
        })
    }
}

/// Host-key verification policy with user-facing labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyChoice {
    Strict,
    AcceptNew,
    Insecure,
}

impl PolicyChoice {
    pub const ALL: [PolicyChoice; 3] = [
        PolicyChoice::Strict,
        PolicyChoice::AcceptNew,
        PolicyChoice::Insecure,
    ];

    fn to_policy(self) -> HostKeyPolicy {
        match self {
            PolicyChoice::Strict => HostKeyPolicy::Strict,
            PolicyChoice::AcceptNew => HostKeyPolicy::AcceptNew,
            PolicyChoice::Insecure => HostKeyPolicy::Insecure,
        }
    }
}

impl fmt::Display for PolicyChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PolicyChoice::Strict => "Estricto (rechaza claves desconocidas)",
            PolicyChoice::AcceptNew => "Aceptar y guardar nuevas",
            PolicyChoice::Insecure => "Inseguro (cualquier clave)",
        })
    }
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
    pub ssh_enabled: bool,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_auth: SshAuthKind,
    pub ssh_password: String,
    pub ssh_key_path: String,
    pub ssh_passphrase: String,
    pub ssh_policy: PolicyChoice,
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
            ssh_enabled: false,
            ssh_host: String::new(),
            ssh_port: "22".to_string(),
            ssh_user: String::new(),
            ssh_auth: SshAuthKind::Password,
            ssh_password: String::new(),
            ssh_key_path: String::new(),
            ssh_passphrase: String::new(),
            ssh_policy: PolicyChoice::AcceptNew,
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
            FormMsg::SshToggled(v) => self.ssh_enabled = v,
            FormMsg::SshHost(v) => self.ssh_host = v,
            FormMsg::SshPort(v) => self.ssh_port = v,
            FormMsg::SshUser(v) => self.ssh_user = v,
            FormMsg::SshAuthKind(v) => self.ssh_auth = v,
            FormMsg::SshPassword(v) => self.ssh_password = v,
            FormMsg::SshKeyPath(v) => self.ssh_key_path = v,
            FormMsg::SshPassphrase(v) => self.ssh_passphrase = v,
            FormMsg::SshPolicy(v) => self.ssh_policy = v,
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

        if self.ssh_enabled {
            cfg = cfg.with_ssh(self.build_ssh()?);
        }
        Ok(cfg)
    }

    /// Validate the tunnel fields and produce the SSH config.
    fn build_ssh(&self) -> Result<SshConfig, String> {
        let host = self.ssh_host.trim();
        if host.is_empty() {
            return Err("indica el host SSH".to_string());
        }
        let port = if self.ssh_port.trim().is_empty() {
            22
        } else {
            self.ssh_port
                .trim()
                .parse()
                .map_err(|_| format!("puerto SSH inválido: {}", self.ssh_port.trim()))?
        };
        let username = self.ssh_user.trim();
        if username.is_empty() {
            return Err("indica el usuario SSH".to_string());
        }
        let auth = match self.ssh_auth {
            SshAuthKind::Password => {
                if self.ssh_password.is_empty() {
                    return Err("indica la contraseña SSH".to_string());
                }
                SshAuth::Password {
                    password: SecretString::new(self.ssh_password.clone()),
                }
            }
            SshAuthKind::Key => {
                let path = self.ssh_key_path.trim();
                if path.is_empty() {
                    return Err("indica la ruta de la clave privada".to_string());
                }
                SshAuth::PrivateKey {
                    path: PathBuf::from(path),
                    passphrase: (!self.ssh_passphrase.is_empty())
                        .then(|| SecretString::new(self.ssh_passphrase.clone())),
                }
            }
            SshAuthKind::Agent => SshAuth::Agent,
        };
        let mut ssh = SshConfig::new(host, username);
        ssh.port = port;
        ssh.auth = auth;
        ssh.policy = self.ssh_policy.to_policy();
        Ok(ssh)
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
        fields = fields.push(
            checkbox(form.ssh_enabled)
                .label("Conectar vía túnel SSH")
                .on_toggle(FormMsg::SshToggled),
        );
        if form.ssh_enabled {
            fields = fields.push(ssh_section(form));
        }
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

/// Bordered sub-panel with the tunnel settings (rendered only when enabled).
fn ssh_section<'a>(form: &ConnForm) -> Element<'a, FormMsg> {
    let auth_fields: Element<'a, FormMsg> = match form.ssh_auth {
        SshAuthKind::Password => labeled(
            "Contraseña SSH",
            text_input("••••••", &form.ssh_password).on_input(FormMsg::SshPassword),
        ),
        SshAuthKind::Key => column![
            labeled(
                "Clave privada",
                text_input("C:/Users/me/.ssh/id_ed25519", &form.ssh_key_path)
                    .on_input(FormMsg::SshKeyPath),
            ),
            labeled(
                "Frase de paso (opcional)",
                text_input("••••••", &form.ssh_passphrase).on_input(FormMsg::SshPassphrase),
            ),
        ]
        .spacing(6)
        .into(),
        SshAuthKind::Agent => text(
            "Se usarán las llaves del agente del sistema (SSH_AUTH_SOCK, \
             agente de OpenSSH en Windows o Pageant). Añade llaves con «ssh-add».",
        )
        .size(11)
        .color(theme::muted())
        .into(),
    };

    let inner = column![
        labeled(
            "Host SSH",
            text_input("bastión", &form.ssh_host).on_input(FormMsg::SshHost),
        ),
        row![
            labeled(
                "Puerto SSH",
                text_input("22", &form.ssh_port).on_input(FormMsg::SshPort),
            ),
            labeled(
                "Usuario SSH",
                text_input("deploy", &form.ssh_user).on_input(FormMsg::SshUser),
            ),
        ]
        .spacing(6),
        labeled(
            "Autenticación",
            pick_list(
                SshAuthKind::ALL,
                Some(form.ssh_auth),
                FormMsg::SshAuthKind
            )
            .width(Length::Fill),
        ),
        auth_fields,
        labeled(
            "Clave del host",
            pick_list(
                PolicyChoice::ALL,
                Some(form.ssh_policy),
                FormMsg::SshPolicy
            )
            .width(Length::Fill),
        ),
    ]
    .spacing(6);

    container(inner).padding(10).style(theme::navigator).into()
}

fn labeled<'a>(label: &'a str, control: impl Into<Element<'a, FormMsg>>) -> Element<'a, FormMsg> {
    column![text(label).size(12), control.into()]
        .spacing(2)
        .width(Length::Fill)
        .into()
}
