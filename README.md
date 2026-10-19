# jorgenio dbstudio

Cliente de bases de datos de escritorio con apariencia de **Navicat**, escrito en
Rust con [iced 0.14](https://iced.rs). Ventana nativa en Windows y Linux, sin
navegador ni runtime embebido.

El nombre visible en la aplicación es **«jorgenio dbstudio»**; los crates,
binarios y rutas conservan el nombre corto `dbstudio`.

## Motores

| Motor | Estado |
| --- | --- |
| SQLite | ✅ |
| PostgreSQL | ✅ (incluye TLS) |
| MySQL / MariaDB | ✅ (incluye TLS) |
| MongoDB | ✅ |
| SQL Server | ✅ |
| Oracle | postergado |

Las conexiones soportan **túnel SSH** (russh) y contraseñas guardadas en el
almacén de claves del sistema (keyring).

## Funciones

- **Conexiones**: formulario con prueba en vivo, lista lateral, selección y
  borrado, múltiples sesiones abiertas.
- **Exploración**: árbol único con buscador, menú contextual por objeto, panel
  de objetos con toolbar, buscador y doble clic, iconos SVG.
- **Editor SQL**: varias pestañas, autocompletado con sugerencias del esquema,
  ejecución con F5, tema claro/oscuro, barra de título propia (sin marco).
- **Resultados**: grid virtual (filas grandes sin fricción), copiar valores
  (con/sin nombres), exportar a CSV/JSON, conteo de filas.
- **Edición de datos**: alta, baja y modificación desde el grid con formulario
  por fila (claves primarias, nulos, tipos), `INSERT`/`UPDATE`/`DELETE`
  generados con el dialecto del motor.
- **Importar datos**: asistente CSV/TSV/JSON con detección de delimitador,
  detección de cabecera, mapeo de columnas, vista previa e inserción por lotes.
- **Diseñador de tablas**: diseño visual con preview del DDL por motor.
- **Diagrama ER** de las tablas de una base.
- **Sincronización de estructura** entre conexiones (comparar y aplicar
  diferencias de esquema).
- **Atajos**: `F5` ejecutar · `Ctrl+Espacio` autocompletar · `Ctrl+C` /
  `Ctrl+Shift+C` copiar valores (con nombres) · `Esc` cerrar popups ·
  `Delete` borrar la conexión seleccionada.

## Requisitos

- **Rust** stable con soporte de la edición 2024 (1.85 o superior).
- **Windows**: Visual C++ Build Tools 2019+ y Windows 10 SDK (linker `link.exe`
  y librerías UCRT/UM). En este repositorio el target vive en `E:/navicat/target`
  (ver `.cargo/config.toml`) para no llenar el disco de `D:`.
- **Linux**: dependencias habituales de build de `iced` y `rusqlite` bundled
  (`build-essential`, `pkg-config`, `libxkbcommon-dev`, `libwayland-dev`, etc.).

## Compilar y ejecutar

Si no partimos de un *Native Tools Command Prompt*, cargamos primero el entorno
MSVC (ajusta las rutas a tu instalación):

```powershell
$msvc = "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Tools\MSVC\14.29.30133"
$kits = "C:\Program Files (x86)\Windows Kits\10"
$env:Path = "$msvc\bin\Hostx64\x64;$kits\bin\10.0.19041.0\x64;$env:USERPROFILE\.cargo\bin;$env:Path"
$env:INCLUDE = "$msvc\include;$kits\Include\10.0.19041.0\ucrt;$kits\Include\10.0.19041.0\um;$kits\Include\10.0.19041.0\shared"
$env:LIB = "$msvc\lib\x64;$kits\Lib\10.0.19041.0\ucrt\x64;$kits\Lib\10.0.19041.0\um\x64"
```

```powershell
cargo run -p dbstudio-app          # desarrollo
cargo test --workspace             # suite completa (core, drivers, app…)
```

## Empaquetar (Windows)

```powershell
powershell -ExecutionPolicy Bypass -File package.ps1
```

Compila en release y genera `dist/jorgenio-dbstudio-<versión>-win64.zip`
(≈15 MB) con el `dbstudio.exe` portable y **sin ventana de consola**.

## Estructura

| Crate | Contenido |
| --- | --- |
| `dbstudio-core` | Modelos compartidos: config, errores, valores, objetos, resultados |
| `dbstudio-secrets` | Contraseñas en el keyring del sistema |
| `dbstudio-ssh` | Túneles SSH (russh) |
| `dbstudio-introspect` | Introspección de esquemas |
| `dbstudio-drivers` | Clientes SQLite/PostgreSQL/MySQL/MariaDB/SQL Server/MongoDB |
| `dbstudio-app` | Aplicación iced: ventanas, grid, editor, paneles |

## Configuración y datos de prueba

- Conexiones guardadas en `%APPDATA%\dbstudio\connections.json`
  (UTF-8 **sin BOM**; un BOM lo rechaza `serde_json`).
- Base de datos de demostración: `sample/demo.db`
  (`cargo run -p dbstudio-drivers --example seed_demo` la regenera con
  `usuarios` y `pedidos`).
- CSV de muestra para el asistente de importación: `sample/import_demo.csv`.

## Estado y hoja de ruta

Completado hasta el hito **m18** (auto-refresh del resultado). Pendientes:
respaldo/dump SQL de tablas, gestión de usuarios y roles, empaquetado para
Linux y soporte de Oracle.

## Licencia

MIT (`workspace.package.license` en `Cargo.toml`).
