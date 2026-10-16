//! Result grid built dynamically from a driver's `QueryResult`.

use dbstudio_core::{CellValue, QueryResult, TypeFamily};
use iced::Task;
use oriel_core::columns::ColumnId;
use oriel_core::data::VecStore;
use oriel_iced::column::TextColumn;
use oriel_iced::grid::{self, BoxedColumn, VirtualGrid};

/// Grid messages are parameterized by the dynamic row type.
pub type GridEvent = grid::Message<GridRow>;

#[derive(Debug, Clone)]
pub struct GridRow {
    pub idx: u64,
    pub cells: Vec<CellValue>,
}

pub type ResultStore = VecStore<GridRow, u64, fn(&GridRow) -> u64>;

fn row_key(r: &GridRow) -> u64 {
    r.idx
}

fn store(rows: Vec<GridRow>) -> ResultStore {
    VecStore::new(rows, row_key as fn(&GridRow) -> u64).with_fields(|_row, _col| None)
}

/// Build a grid whose columns mirror the result set.
pub fn build(result: QueryResult) -> (VirtualGrid<ResultStore>, Task<grid::Message<GridRow>>) {
    let rows: Vec<GridRow> = result
        .rows
        .into_iter()
        .enumerate()
        .map(|(i, cells)| GridRow {
            idx: i as u64,
            cells,
        })
        .collect();
    let columns: Vec<BoxedColumn<GridRow>> = result
        .columns
        .iter()
        .enumerate()
        .map(|(i, meta)| {
            let numeric = matches!(
                meta.data_type.family,
                TypeFamily::Int | TypeFamily::UInt | TypeFamily::Float | TypeFamily::Decimal
            );
            let width = (meta.name.len() as f32 * 8.0 + 48.0).clamp(90.0, 340.0);
            let col = TextColumn::new(
                ColumnId((i + 1) as u64),
                meta.name.clone(),
                width,
                move |r: &GridRow| {
                    r.cells
                        .get(i)
                        .map(|c| c.as_display().into_owned())
                        .unwrap_or_default()
                },
            );
            let col = if numeric { col.numeric() } else { col };
            Box::new(col) as BoxedColumn<GridRow>
        })
        .collect();
    // Keep the built-in precedence (selected > focus > hover > zebra) but
    // pin the selection to a solid Navicat-style blue instead of the
    // palette-derived tint.
    let (mut grid, init) = VirtualGrid::new(store(rows), columns);
    grid.set_row_style(|theme, ctx| {
        let mut style = grid::default_row_style(theme, ctx);
        if ctx.is_selected {
            style.background = Some(iced::color!(0xd3e2fb).into());
        }
        style
    });
    (grid, init)
}

/// Placeholder grid shown before the first query.
pub fn empty() -> VirtualGrid<ResultStore> {
    let (grid, init) = VirtualGrid::new(store(Vec::new()), Vec::new());
    let _ = init;
    grid
}
