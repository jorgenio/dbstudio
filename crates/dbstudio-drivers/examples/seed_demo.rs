//! Seeds `sample/demo.db` with a small but non-trivial schema used for
//! manual end-to-end runs of the app. Uses the driver stack itself.

use dbstudio_core::{ConnectionConfig, Engine, ExecuteRequest};
use dbstudio_drivers::connect;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = format!("{}/../../sample", env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(&dir)?;
    let path = format!("{dir}/demo.db");
    if std::path::Path::new(&path).exists() {
        std::fs::remove_file(&path)?;
    }

    let cfg = ConnectionConfig::new("demo", Engine::Sqlite).with_host(&path);
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        let session = connect(&cfg).await?;

        let schema = "
            CREATE TABLE usuarios (
                id INTEGER PRIMARY KEY,
                nombre TEXT NOT NULL,
                email TEXT NOT NULL,
                activo INTEGER NOT NULL DEFAULT 1,
                saldo REAL,
                creado TEXT
            );
            CREATE TABLE pedidos (
                id INTEGER PRIMARY KEY,
                usuario_id INTEGER NOT NULL REFERENCES usuarios(id),
                total REAL NOT NULL,
                estado TEXT NOT NULL
            );
            CREATE VIEW usuarios_activos AS
                SELECT id, nombre, email, saldo FROM usuarios WHERE activo = 1;
        ";
        session
            .client
            .execute(ExecuteRequest::new(schema))
            .await?;

        let mut values = Vec::with_capacity(5000);
        for id in 1..=5000u64 {
            values.push(format!(
                "({id}, 'usuario_{id}', 'user{id}@example.com', {}, {:.2}, '2026-{:02}-{:02}')",
                i64::from(id % 7 != 0),
                (id as f64) * 1.37 % 10_000.0,
                1 + id % 12,
                1 + id % 28,
            ));
        }
        session
            .client
            .execute(ExecuteRequest::new(format!(
                "INSERT INTO usuarios (id, nombre, email, activo, saldo, creado) VALUES {}",
                values.join(",")
            )))
            .await?;

        let mut pedidos = Vec::with_capacity(2000);
        for id in 1..=2000u64 {
            pedidos.push(format!(
                "({id}, {}, {:.2}, '{}')",
                1 + id % 5000,
                5.0 + (id as f64) * 3.19 % 500.0,
                ["pendiente", "enviado", "cancelado"][id as usize % 3],
            ));
        }
        session
            .client
            .execute(ExecuteRequest::new(format!(
                "INSERT INTO pedidos (id, usuario_id, total, estado) VALUES {}",
                pedidos.join(",")
            )))
            .await?;

        let check = session
            .client
            .execute(ExecuteRequest::new(
                "SELECT COUNT(*) AS usuarios, (SELECT COUNT(*) FROM pedidos) AS pedidos FROM usuarios",
            ))
            .await?;
        let summary = check
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.as_display().into_owned())
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .collect::<Vec<_>>()
            .join(", ");

        let tree = session
            .client
            .list_children(&dbstudio_core::ObjectRef::root(cfg.id))
            .await?;
        let labels = tree
            .iter()
            .map(|n| format!("{}{}", n.label, if n.has_children { "/" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ");

        Ok::<_, dbstudio_core::Error>(format!(
            "usuarios/pedidos = {summary}; árbol = {labels}"
        ))
    })?;
    println!("seeded {path}: {result}");
    Ok(())
}
