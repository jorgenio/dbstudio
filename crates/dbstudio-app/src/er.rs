//! Diagrama entidad-relación: descubrimiento de tablas, layout reticular y
//! dibujo en un canvas de iced.
//!
//! `load` recorre el árbol hasta los nodos `Table` (bajando por niveles
//! Database/Schema) y pide `describe_table` a cada uno; `Diagram::new`
//! deriva las aristas de los `foreign_keys` devueltos por cada driver y
//! calcula la caja de cada nodo. El canvas pinta el resultado adaptándose
//! al tema activo en el momento del dibujo.

use std::collections::HashMap;

use iced::advanced::text::Alignment as TextAlign;
use iced::widget::canvas;
use iced::{Pixels, Point, Rectangle, Size, Theme};

use dbstudio_core::{ColumnMeta, Error, ObjectKind, ObjectNode, ObjectRef, Result};
use dbstudio_drivers::{DatabaseClient, TableDetail};

use crate::theme;

/// Node width in logical pixels (layout and text use it).
pub const NODE_WIDTH: f32 = 210.0;
const HEADER_H: f32 = 28.0;
const ROW_H: f32 = 17.0;
const H_GAP: f32 = 76.0;
const V_GAP: f32 = 56.0;
const MARGIN: f32 = 30.0;
/// Column rows drawn per node before collapsing into a "+N más" footnote.
const ROWS_SHOWN: usize = 9;
/// Tables loaded per diagram (bounded so a huge schema stays responsive).
const MAX_TABLES: usize = 80;
const DOT_STEP: usize = 28;

/// One table of the diagram with the metadata the drawing needs.
#[derive(Clone)]
pub struct ErTable {
    pub label: String,
    pub columns: Vec<ColumnMeta>,
    pub primary_keys: Vec<String>,
    pub foreign_keys: Vec<dbstudio_drivers::ForeignKeyMeta>,
}

impl ErTable {
    pub fn new(label: impl Into<String>, detail: TableDetail) -> Self {
        Self {
            label: label.into(),
            columns: detail.columns,
            primary_keys: detail.primary_keys,
            foreign_keys: detail.foreign_keys,
        }
    }

    fn is_pk(&self, column: &str) -> bool {
        self.primary_keys.iter().any(|k| k.eq_ignore_ascii_case(column))
    }

    fn is_fk(&self, column: &str) -> bool {
        self.foreign_keys
            .iter()
            .any(|fk| fk.columns.iter().any(|c| c.eq_ignore_ascii_case(column)))
    }

    /// `PK` / `FK` / `·` marker shown left of every column name.
    fn marker(&self, column: &str) -> Marker {
        if self.is_pk(column) {
            Marker::Pk
        } else if self.is_fk(column) {
            Marker::Fk
        } else {
            Marker::Plain
        }
    }

    fn truncated(&self) -> bool {
        self.columns.len() > ROWS_SHOWN
    }
}

enum Marker {
    Pk,
    Fk,
    Plain,
}

/// Grid rectangle of a node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NodeBox {
    fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Directed FK edge: `from` table → `to` (referenced) table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErEdge {
    pub from: usize,
    pub to: usize,
    /// `local cols → referenced cols`, e.g. `"cliente_id → id"`.
    pub label: String,
}

/// Result of loading a diagram: ready-to-draw tables (or a failure).
#[derive(Clone)]
pub struct ErPayload {
    pub connection: dbstudio_core::ConnectionId,
    pub tables: Vec<ErTable>,
    pub failed: Vec<String>,
}

/// Height of a node given how many column rows it shows.
fn node_height(table: &ErTable) -> f32 {
    let mut rows = table.columns.len().min(ROWS_SHOWN) as f32;
    if table.truncated() {
        rows += 1.0;
    }
    HEADER_H + rows * ROW_H + 10.0
}

/// Square-ish grid layout: `ceil(sqrt(n))` columns, row height = tallest node.
pub fn layout(tables: &[ErTable]) -> (Vec<NodeBox>, (f32, f32)) {
    if tables.is_empty() {
        return (Vec::new(), (MARGIN * 2.0, MARGIN * 2.0));
    }
    let cols = f32::sqrt(tables.len() as f32).ceil().max(1.0) as usize;
    let mut boxes = Vec::with_capacity(tables.len());
    let mut x = MARGIN;
    let mut y = MARGIN;
    let mut row_h = 0.0f32;
    for (i, table) in tables.iter().enumerate() {
        if i != 0 && i % cols == 0 {
            y += row_h + V_GAP;
            x = MARGIN;
            row_h = 0.0;
        }
        let h = node_height(table);
        boxes.push(NodeBox {
            x,
            y,
            w: NODE_WIDTH,
            h,
        });
        row_h = row_h.max(h);
        x += NODE_WIDTH + H_GAP;
    }
    let w = MARGIN * 2.0 + cols as f32 * NODE_WIDTH + (cols as f32 - 1.0) * H_GAP;
    let h = boxes.last().map_or(MARGIN * 2.0, |b| b.y + b.h + MARGIN);
    (boxes, (w, h))
}

/// FK edges between known tables. References are matched case-insensitively
/// by table label, tolerating a schema prefix (`public.usuarios` →
/// `usuarios`); unknown targets and self-references are dropped.
pub fn build_edges(tables: &[ErTable]) -> Vec<ErEdge> {
    let mut edges = Vec::new();
    for (i, table) in tables.iter().enumerate() {
        for fk in &table.foreign_keys {
            let target = fk
                .ref_table
                .rsplit('.')
                .next()
                .unwrap_or(&fk.ref_table)
                .to_lowercase();
            let Some(j) = tables
                .iter()
                .position(|t| t.label.to_lowercase() == target)
            else {
                continue;
            };
            if j == i {
                continue;
            }
            edges.push(ErEdge {
                from: i,
                to: j,
                label: format!("{} → {}", fk.columns.join(", "), fk.ref_columns.join(", ")),
            });
        }
    }
    edges
}

/// Drawn diagram: tables + boxes + edges, sized for its scrollable canvas.
pub struct Diagram {
    pub tables: Vec<ErTable>,
    pub boxes: Vec<NodeBox>,
    pub edges: Vec<ErEdge>,
    pub size: (f32, f32),
}

impl Diagram {
    pub fn new(tables: Vec<ErTable>) -> Self {
        let edges = build_edges(&tables);
        let (boxes, size) = layout(&tables);
        Self {
            tables,
            boxes,
            edges,
            size,
        }
    }

    fn paint(&self, frame: &mut canvas::Frame, palette: &theme::ErPalette) {
        let total = frame.size();
        self.paint_dots(frame, palette, total);
        self.paint_edges(frame, palette);
        self.paint_nodes(frame, palette);
    }

    /// Faint dot grid so the canvas reads as a diagram sheet.
    fn paint_dots(&self, frame: &mut canvas::Frame, palette: &theme::ErPalette, total: Size) {
        let mut y = MARGIN / 2.0;
        while y < total.height {
            let mut x = MARGIN / 2.0;
            while x < total.width {
                frame.fill(
                    &canvas::Path::circle(Point::new(x, y), 0.9),
                    palette.dot,
                );
                x += DOT_STEP as f32;
            }
            y += DOT_STEP as f32;
        }
    }

    /// Bezier curves from each FK table to its referenced table, with an
    /// arrowhead at the target and the column pairing over the midpoint.
    fn paint_edges(&self, frame: &mut canvas::Frame, palette: &theme::ErPalette) {
        let mut totals: HashMap<(usize, usize), usize> = HashMap::new();
        for e in &self.edges {
            *totals.entry((e.from, e.to)).or_insert(0) += 1;
        }
        let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
        for e in &self.edges {
            let (Some(s), Some(t)) = (self.boxes.get(e.from), self.boxes.get(e.to)) else {
                continue;
            };
            let key = (e.from, e.to);
            let total = totals.get(&key).copied().unwrap_or(1).max(1);
            let k = seen.entry(key).or_insert(0);
            // Fan parallel FKs between the same pair instead of overlapping.
            let shift = (*k as f32 - (total - 1) as f32 / 2.0) * 15.0;
            *k += 1;

            let from_c = s.center();
            let to_c = t.center();
            let go_right = to_c.x >= from_c.x;
            let start = Point::new(
                if go_right { s.x + s.w } else { s.x },
                from_c.y + shift,
            );
            let end = Point::new(if go_right { t.x } else { t.x + t.w }, to_c.y + shift);
            let dx = (end.x - start.x).abs().max(60.0) * 0.45;
            let (c1, c2) = if go_right {
                (
                    Point::new(start.x + dx, start.y),
                    Point::new(end.x - dx, end.y),
                )
            } else {
                (
                    Point::new(start.x - dx, start.y),
                    Point::new(end.x + dx, end.y),
                )
            };

            let curve = canvas::Path::new(|b| {
                b.move_to(start);
                b.bezier_curve_to(c1, c2, end);
            });
            frame.stroke(
                &curve,
                canvas::Stroke::default()
                    .with_color(palette.edge)
                    .with_width(1.7),
            );

            // Arrowhead along the final tangent (end − c2).
            let (dir_x, dir_y) = {
                let (x, y) = (end.x - c2.x, end.y - c2.y);
                let len = (x * x + y * y).sqrt();
                if len < 0.001 {
                    (if go_right { 1.0 } else { -1.0 }, 0.0)
                } else {
                    (x / len, y / len)
                }
            };
            let (perp_x, perp_y) = (-dir_y, dir_x);
            let base_x = end.x - dir_x * 9.0;
            let base_y = end.y - dir_y * 9.0;
            let arrow = canvas::Path::new(|b| {
                b.move_to(end);
                b.line_to(Point::new(
                    base_x + perp_x * 4.5,
                    base_y + perp_y * 4.5,
                ));
                b.line_to(Point::new(
                    base_x - perp_x * 4.5,
                    base_y - perp_y * 4.5,
                ));
                b.close();
            });
            frame.fill(&arrow, palette.edge);

            // Bezier midpoint at t=0.5, nudged above the curve.
            let mid = Point::new(
                (start.x + 3.0 * c1.x + 3.0 * c2.x + end.x) / 8.0,
                (start.y + 3.0 * c1.y + 3.0 * c2.y + end.y) / 8.0 - 12.0,
            );
            frame.fill_text(canvas::Text {
                content: e.label.clone(),
                position: mid,
                max_width: NODE_WIDTH + H_GAP,
                color: palette.muted,
                size: Pixels(10.0),
                align_x: TextAlign::Center,
                align_y: iced::alignment::Vertical::Center,
                ..Default::default()
            });
        }
    }

    /// Node cards: body, header band with the table name, column rows with
    /// PK/FK markers and right-aligned types, plus an outline on top.
    fn paint_nodes(&self, frame: &mut canvas::Frame, palette: &theme::ErPalette) {
        for (i, table) in self.tables.iter().enumerate() {
            let Some(bx) = self.boxes.get(i) else {
                continue;
            };
            let body = canvas::Path::rounded_rectangle(
                Point::new(bx.x, bx.y),
                Size::new(bx.w, bx.h),
                6.0.into(),
            );
            frame.fill(&body, palette.node_bg);

            // Header band: rounded on top, squared off at the bottom.
            let header = canvas::Path::rounded_rectangle(
                Point::new(bx.x, bx.y),
                Size::new(bx.w, HEADER_H),
                6.0.into(),
            );
            frame.fill(&header, palette.header);
            let flatten = canvas::Path::rectangle(
                Point::new(bx.x, bx.y + HEADER_H - 7.0),
                Size::new(bx.w, 7.0),
            );
            frame.fill(&flatten, palette.header);

            frame.fill_text(canvas::Text {
                content: ellipsize(&table.label, 26),
                position: Point::new(bx.x + 10.0, bx.y + HEADER_H / 2.0),
                max_width: bx.w - 20.0,
                color: palette.header_text,
                size: Pixels(13.0),
                align_y: iced::alignment::Vertical::Center,
                ..Default::default()
            });

            let shown = table.columns.len().min(ROWS_SHOWN);
            for (row, col) in table.columns.iter().take(shown).enumerate() {
                let y = bx.y + HEADER_H + 10.0 + row as f32 * ROW_H;
                let marker = table.marker(&col.name);
                let (mark_text, mark_color) = match marker {
                    Marker::Pk => ("PK", palette.marker_pk),
                    Marker::Fk => ("FK", palette.marker_fk),
                    Marker::Plain => ("·", palette.muted),
                };
                frame.fill_text(canvas::Text {
                    content: mark_text.to_string(),
                    position: Point::new(bx.x + 10.0, y),
                    max_width: 22.0,
                    color: mark_color,
                    size: Pixels(10.0),
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
                frame.fill_text(canvas::Text {
                    content: ellipsize(&col.name, 15),
                    position: Point::new(bx.x + 36.0, y),
                    max_width: bx.w - 116.0,
                    color: palette.ink,
                    size: Pixels(11.0),
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
                frame.fill_text(canvas::Text {
                    content: ellipsize(&col.data_type.name, 10),
                    position: Point::new(bx.x + bx.w - 8.0, y),
                    max_width: 70.0,
                    color: palette.muted,
                    size: Pixels(10.0),
                    align_x: TextAlign::Right,
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
            }
            if table.truncated() {
                let hidden = table.columns.len() - shown;
                let y = bx.y + HEADER_H + 10.0 + shown as f32 * ROW_H;
                frame.fill_text(canvas::Text {
                    content: format!("+{hidden} columnas…"),
                    position: Point::new(bx.x + 36.0, y),
                    max_width: bx.w - 44.0,
                    color: palette.muted,
                    size: Pixels(10.0),
                    align_y: iced::alignment::Vertical::Center,
                    ..Default::default()
                });
            }

            frame.stroke(
                &body,
                canvas::Stroke::default()
                    .with_color(palette.border)
                    .with_width(1.0),
            );
        }
    }
}

/// `Component::method` naming in table names (long identifiers stay readable).
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

impl<Message> canvas::Program<Message> for Diagram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let palette = theme::er_palette(theme::is_dark(theme));
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        self.paint(&mut frame, &palette);
        vec![frame.into_geometry()]
    }
}

/// Walk the object tree down to `Table` nodes (descending through at most
/// three Database/Schema levels) and describe each one.
pub async fn load(
    client: &dyn DatabaseClient,
    root: &ObjectRef,
) -> Result<(Vec<ErTable>, Vec<String>)> {
    let mut level = client
        .list_children(root)
        .await
        .map_err(|e| Error::Query(e.to_string()))?;
    let mut nodes: Vec<ObjectNode> = Vec::new();
    for _ in 0..3 {
        let mut tables = Vec::new();
        let mut containers = Vec::new();
        for node in level {
            match node.kind {
                ObjectKind::Table => tables.push(node),
                ObjectKind::Database | ObjectKind::Schema if node.has_children => {
                    containers.push(node)
                }
                _ => {}
            }
        }
        nodes.append(&mut tables);
        if containers.is_empty() {
            break;
        }
        level = Vec::new();
        for container in containers {
            let kids = client
                .list_children(&container.object)
                .await
                .map_err(|e| Error::Query(e.to_string()))?;
            level.extend(kids);
        }
    }

    let total = nodes.len();
    let mut failed = Vec::new();
    if total > MAX_TABLES {
        failed.push(format!("mostrando {MAX_TABLES} de {total} tablas"));
    }
    let mut tables = Vec::new();
    for node in nodes.into_iter().take(MAX_TABLES) {
        match client.describe_table(&node.object).await {
            Ok(detail) => tables.push(ErTable::new(node.label, detail)),
            Err(e) => failed.push(format!("{}: {e}", node.label)),
        }
    }
    Ok((tables, failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DataType;
    use dbstudio_drivers::ForeignKeyMeta;

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            data_type: DataType::new("integer", dbstudio_core::TypeFamily::Int),
            nullable: true,
            comment: None,
        }
    }

    fn table(label: &str, columns: &[&str], fks: Vec<ForeignKeyMeta>) -> ErTable {
        ErTable {
            label: label.into(),
            columns: columns.iter().map(|c| col(c)).collect(),
            primary_keys: vec!["id".into()],
            foreign_keys: fks,
        }
    }

    fn fk(local: &str, ref_table: &str, ref_col: &str) -> ForeignKeyMeta {
        ForeignKeyMeta {
            name: Some(format!("fk_{local}")),
            columns: vec![local.into()],
            ref_table: ref_table.into(),
            ref_columns: vec![ref_col.into()],
            on_delete: None,
            on_update: None,
        }
    }

    #[test]
    fn layout_uses_a_square_grid_with_gaps() {
        let tables: Vec<ErTable> = ["a", "b", "c", "d"]
            .iter()
            .map(|l| table(l, &["id"], Vec::new()))
            .collect();
        let (boxes, size) = layout(&tables);
        assert_eq!(boxes.len(), 4);
        assert_eq!(boxes[0].x, MARGIN);
        assert_eq!(boxes[0].y, MARGIN);
        // 4 tables → 2 columns: second node to the right, third below.
        assert!(boxes[1].x > boxes[0].x);
        assert!(boxes[2].y > boxes[0].y + boxes[0].h);
        // Nodes of a row keep the horizontal gap.
        assert_eq!(
            boxes[1].x - (boxes[0].x + boxes[0].w),
            H_GAP,
            "horizontal gap"
        );
        let max_x = boxes.iter().map(|b| b.x + b.w).fold(0.0, f32::max);
        let max_y = boxes.iter().map(|b| b.y + b.h).fold(0.0, f32::max);
        assert!(size.0 >= max_x + MARGIN, "width covers every node");
        assert!(size.1 >= max_y + MARGIN, "height covers every node");
    }

    #[test]
    fn layout_grows_with_hidden_columns() {
        let short = table("s", &["id"], Vec::new());
        let many_cols: Vec<&str> = (0..20).map(|_| "c").collect();
        let tall = table("t", &many_cols, Vec::new());
        assert!(node_height(&tall) > node_height(&short));
        // Collapsed rows still fit: height stops growing past ROWS_SHOWN.
        let even_more: Vec<&str> = (0..40).map(|_| "c").collect();
        let huge = table("h", &even_more, Vec::new());
        assert_eq!(
            node_height(&huge),
            node_height(&tall),
            "only +N footnote grows"
        );
    }

    #[test]
    fn edges_match_targets_case_insensitively_and_skip_unknowns() {
        let tables = vec![
            table("Pedidos", &["id", "cliente_id"], vec![fk("cliente_id", "Clientes", "id")]),
            table("clientes", &["id"], Vec::new()),
            table(
                "Lineas",
                &["id", "pedido_id"],
                vec![
                    fk("pedido_id", "PEDIDOS", "id"),
                    fk("pedido_id", "fantasma", "id"),
                ],
            ),
        ];
        let edges = build_edges(&tables);
        assert_eq!(edges.len(), 2, "unknown target dropped");
        assert_eq!(edges[0].from, 0);
        assert_eq!(edges[0].to, 1);
        assert_eq!(edges[0].label, "cliente_id → id");
        assert_eq!(edges[1].from, 2);
        assert_eq!(edges[1].to, 0);
    }

    #[test]
    fn schema_qualified_targets_resolve_to_the_basename() {
        let tables = vec![
            table("usuarios", &["id"], Vec::new()),
            table(
                "auditoria",
                &["id", "usuario_id"],
                vec![fk("usuario_id", "public.usuarios", "id")],
            ),
        ];
        let edges = build_edges(&tables);
        assert_eq!(edges.len(), 1);
        assert_eq!((edges[0].from, edges[0].to), (1, 0));
    }

    #[test]
    fn self_references_are_not_drawn() {
        let tables = vec![table("nodos", &["id", "padre_id"], vec![fk("padre_id", "nodos", "id")])];
        assert!(build_edges(&tables).is_empty());
    }

    #[test]
    fn markers_distinguish_pk_fk_and_plain() {
        let t = table(
            "pedidos",
            &["id", "cliente_id", "fecha"],
            vec![fk("cliente_id", "clientes", "id")],
        );
        assert!(matches!(t.marker("id"), Marker::Pk));
        assert!(matches!(t.marker("cliente_id"), Marker::Fk));
        assert!(matches!(t.marker("fecha"), Marker::Plain));
        assert!(matches!(t.marker("CLIENTE_ID"), Marker::Fk), "case-insensitive");
    }

    #[test]
    fn diagram_starts_sized_for_the_canvas() {
        let tables = vec![table("a", &["id"], Vec::new())];
        let d = Diagram::new(tables);
        assert!(!d.tables.is_empty());
        assert!(d.size.0 > NODE_WIDTH, "room for margins");
        assert!(d.size.1 > HEADER_H);
        assert!(Diagram::new(Vec::new()).tables.is_empty());
    }

    #[tokio::test]
    async fn load_walks_the_sample_database_and_finds_its_foreign_key() {
        let path = format!("{}/../../sample/demo.db", env!("CARGO_MANIFEST_DIR"));
        assert!(std::path::Path::new(&path).exists(), "sample/demo.db");
        let cfg = dbstudio_core::ConnectionConfig::new(
            "demo",
            dbstudio_core::Engine::Sqlite,
        )
        .with_host(&path);
        let session = dbstudio_drivers::connect(&cfg).await.expect("connect");
        let (tables, failed) = load(&*session.client, &ObjectRef::root(cfg.id))
            .await
            .expect("load");
        assert!(failed.is_empty(), "{failed:?}");
        let names: Vec<&str> = tables.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(names, vec!["pedidos", "usuarios"], "views are skipped");

        let pedidos = &tables[0];
        assert_eq!(pedidos.foreign_keys.len(), 1);
        let fk = &pedidos.foreign_keys[0];
        assert_eq!(fk.columns, vec!["usuario_id"]);
        assert_eq!(fk.ref_table, "usuarios");
        assert_eq!(fk.ref_columns, vec!["id"]);
        assert_eq!(pedidos.primary_keys, vec!["id"]);

        let diagram = Diagram::new(tables);
        assert_eq!(diagram.edges.len(), 1, "pedidos → usuarios");
        assert_eq!(diagram.edges[0].from, 0);
        assert_eq!(diagram.edges[0].to, 1);
        assert_eq!(diagram.edges[0].label, "usuario_id → id");
    }
}
