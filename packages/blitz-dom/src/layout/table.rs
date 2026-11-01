use std::{ops::Range, sync::Arc};

use markup5ever::local_name;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{Atom, computed_values::table_layout::T as TableLayout};
use taffy::{
    Dimension, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, compute_leaf_layout,
    style_helpers,
};

use crate::{BaseDocument, Node};

use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

pub struct TableTreeWrapper<'doc> {
    pub(crate) doc: &'doc mut BaseDocument,
    pub(crate) ctx: Arc<TableContext>,
}

#[derive(Debug, Clone)]
pub struct TableContext {
    style: taffy::Style<Atom>,
    items: Vec<TableItem>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TableItemKind {
    Row,
    Cell,
}

#[derive(Debug, Clone)]
pub struct TableItem {
    kind: TableItemKind,
    node_id: usize,
    style: taffy::Style<Atom>,
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: usize,
) -> (TableContext, Vec<usize>) {
    let mut items: Vec<TableItem> = Vec::new();
    let mut row = 0u16;
    let mut col = 0u16;

    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    drop(stylo_styles);

    let mut column_sizes = ColumnWidths::default();
    let mut row_spans = RowSpans::default();
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
            is_fixed,
            &mut row,
            &mut col,
            &mut items,
            &mut column_sizes,
            &mut row_spans,
        );
    }
    // Rows outside any row group end with the table.
    row_spans.end_group(&mut items, row);
    style.grid_template_columns = column_sizes
        .into_tracks(col)
        .into_iter()
        .map(|dim| TrackSizingFunction::from(dim).into())
        .collect();
    style.grid_template_rows = vec![style_helpers::auto(); row as usize];

    let layout_children = items
        .iter()
        .filter(|item| item.kind == TableItemKind::Cell)
        .map(|cell| cell.node_id)
        .collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (TableContext { style, items }, layout_children)
}

/// The width of each column: its column element's (`col`, or a
/// `colgroup` without `col` children), or else that of the first-row cell
/// starting in it (CSS 2.1 §17.5.2).
#[derive(Debug, Default)]
pub(crate) struct ColumnWidths {
    /// Each column's width, `auto` where none is set.
    widths: Vec<Dimension>,
}

impl ColumnWidths {
    /// Adds the `span` columns of a column element whose `width` is `width`.
    fn add_columns(&mut self, width: Dimension, span: u16) {
        self.widths
            .extend(std::iter::repeat_n(width, usize::from(span)));
    }

    /// Sizes the column at `col` by a cell of the first row, unless a column
    /// element sized it.
    fn first_row_cell(&mut self, col: u16, width: Dimension) {
        let col = usize::from(col);
        if self.widths.len() <= col {
            self.widths.resize(col + 1, style_helpers::auto());
        }
        if self.widths[col].is_auto() {
            self.widths[col] = width;
        }
    }

    /// Widens the column at `col` to a later row's cell `len` wide, in
    /// automatic table layout; a percentage column keeps its width.
    fn later_row_cell(&mut self, col: u16, len: f32) {
        let Some(width) = self.widths.get_mut(usize::from(col)) else {
            return;
        };
        *width = match width.tag() {
            taffy::CompactLength::LENGTH_TAG => style_helpers::length(width.value().max(len)),
            taffy::CompactLength::AUTO_TAG => style_helpers::length(len),
            _ => *width,
        };
    }

    /// The tracks of a table of `count` columns.
    fn into_tracks(mut self, count: u16) -> Vec<Dimension> {
        self.widths
            .resize(usize::from(count), style_helpers::auto());
        self.widths
    }
}

/// The cells of the current row group that span more than one row.
#[derive(Debug, Default)]
pub(crate) struct RowSpans {
    /// For each column, the last row that a cell from an earlier row covers.
    covered_through: Vec<u16>,
    /// `(item index, first row, rowspan)` of each spanning cell.
    cells: Vec<(usize, u16, u16)>,
}

impl RowSpans {
    /// The first column at or after `col` that no cell of an earlier row
    /// covers in `row`.
    fn free_column(&self, row: u16, mut col: u16) -> u16 {
        while self
            .covered_through
            .get(col as usize)
            .is_some_and(|&last| last >= row)
        {
            col += 1;
        }
        col
    }

    /// Records the cell at `item` covering `columns` from `row` down
    /// `rowspan` rows.
    fn span(&mut self, item: usize, row: u16, columns: Range<u16>, rowspan: u16) {
        let last = row.saturating_add(rowspan - 1);
        if self.covered_through.len() < columns.end as usize {
            self.covered_through.resize(columns.end as usize, 0);
        }
        for col in columns {
            self.covered_through[col as usize] = last;
        }
        self.cells.push((item, row, rowspan));
    }

    /// Ends a row group whose last row is `last_row`: each spanning cell
    /// spans its rows up to the group's end, and no later row is covered.
    fn end_group(&mut self, items: &mut [TableItem], last_row: u16) {
        for (item, row, rowspan) in self.cells.drain(..) {
            items[item].style.grid_row.end = style_helpers::span(rowspan.min(last_row - row + 1));
        }
        self.covered_through.clear();
    }
}

pub(crate) fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: usize,
    is_fixed: bool,
    row: &mut u16,
    col: &mut u16,
    cells: &mut Vec<TableItem>,
    columns: &mut ColumnWidths,
    row_spans: &mut RowSpans,
) {
    let node = &doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
        println!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(doc, child_id, is_fixed, row, col, cells, columns, row_spans);
            }
            doc.nodes[node_id].children = children;
            // A row span ends with its row group (css-tables-3).
            if display.inside() != DisplayInside::Contents {
                row_spans.end_group(cells, *row);
            }
        }
        DisplayInside::TableRow => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            *row += 1;
            *col = 0;

            {
                let stylo_style = &node.primary_styles().unwrap();
                let mut style = stylo_taffy::to_taffy_style(stylo_style);
                style.grid_column = taffy::Line {
                    start: style_helpers::line(0),
                    end: style_helpers::line(-1),
                };
                style.grid_row = taffy::Line {
                    start: style_helpers::line(*row as i16),
                    end: style_helpers::span(1),
                };
                cells.push(TableItem {
                    kind: TableItemKind::Row,
                    node_id,
                    style,
                });
            }

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(doc, child_id, is_fixed, row, col, cells, columns, row_spans);
            }
            doc.nodes[node_id].children = children;
            // Count the columns that spans from above cover at the row's end.
            *col = row_spans.free_column(*row, *col);
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse().ok())
                .unwrap_or(1);
            // Clamped as the HTML Standard does; `rowspan="0"` is read as 1.
            let rowspan: u16 = node
                .attr(local_name!("rowspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.clamp(1, 65534))
                .unwrap_or(1);
            // A cell starts at the first column no cell of an earlier row
            // spans into.
            *col = row_spans.free_column(*row, *col);
            let mut style = stylo_taffy::to_taffy_style(stylo_style);

            // TODO: account for padding/border/margin
            if *row == 1 {
                let column = match style.size.width.tag() {
                    taffy::CompactLength::LENGTH_TAG => {
                        let len = style.size.width.value();
                        let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                        style_helpers::length(len + padding.left + padding.right)
                    }
                    taffy::CompactLength::PERCENT_TAG => {
                        style_helpers::percent(style.size.width.value())
                    }
                    taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                    _ => unreachable!(),
                };
                columns.first_row_cell(*col, column);
            } else if !is_fixed && taffy::CompactLength::LENGTH_TAG == style.size.width.tag() {
                columns.later_row_cell(*col, style.size.width.value());
            }

            style.grid_column = taffy::Line {
                start: style_helpers::line((*col + 1) as i16),
                end: style_helpers::span(colspan),
            };
            style.grid_row = taffy::Line {
                start: style_helpers::line(*row as i16),
                end: style_helpers::span(1),
            };
            style.size.width = style_helpers::auto();
            if rowspan > 1 {
                row_spans.span(cells.len(), *row, *col..*col + colspan, rowspan);
            }
            cells.push(TableItem {
                kind: TableItemKind::Cell,
                node_id,
                style,
            });

            *col += colspan;
        }
        DisplayInside::Flow
        | DisplayInside::FlowRoot
        | DisplayInside::Flex
        | DisplayInside::Grid => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Probably a table caption: ignore
            // println!(
            //     "Warning: ignoring non-table typed descendent of table ({:?})",
            //     display.inside()
            // );
        }
        DisplayInside::TableColumn => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let (width, span) = column_of(node);
            columns.add_columns(width, span);
        }
        DisplayInside::TableColumnGroup => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let mut children: Vec<_> = node
                .children
                .iter()
                .map(|&child_id| &doc.nodes[child_id])
                .filter(|child| {
                    child.primary_styles().is_some_and(|style| {
                        let display = style.clone_display();
                        display.outside() != DisplayOutside::None
                            && display.inside() == DisplayInside::TableColumn
                    })
                })
                .map(column_of)
                .collect();
            // A column group without columns spans `span` columns itself
            // (HTML, "forming a table").
            if children.is_empty() {
                children.push(column_of(node));
            }
            for (width, span) in children {
                columns.add_columns(width, span);
            }
        }
        DisplayInside::Table => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            //Ignore
        }
        DisplayInside::None => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Ignore
        }
    }
}

/// The `width` and `span` of a column element, whose span is clamped to
/// 1 to 1000 as the HTML Standard does.
fn column_of(node: &Node) -> (Dimension, u16) {
    let width = node
        .primary_styles()
        .map_or_else(style_helpers::auto, |style| {
            stylo_taffy::convert::dimension(&style.get_position().width)
        });
    let span = node
        .attr(local_name!("span"))
        .and_then(|val| val.parse::<u16>().ok())
        .map_or(1, |v| v.clamp(1, 1000));
    (width, span)
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.items.len())
    }

    #[inline(always)]
    fn child_count(&self, node_id: taffy::NodeId) -> usize {
        self.doc.child_count(node_id)
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = taffy::NodeId::from(self.ctx.items[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.items[usize::from(node_id)];
        match cell.kind {
            TableItemKind::Row => {
                compute_leaf_layout(inputs, &cell.style, resolve_calc_value, |_, _| {
                    taffy::Size::ZERO
                })
            }
            TableItemKind::Cell => {
                let node_id = taffy::NodeId::from(cell.node_id);
                self.doc.compute_child_layout(node_id, inputs)
            }
        }
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.items[usize::from(child_node_id)].style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> TableItem {
        TableItem {
            kind: TableItemKind::Cell,
            node_id: 0,
            style: taffy::Style::default(),
        }
    }

    #[test]
    fn a_column_elements_width_comes_before_its_first_row_cells() {
        let mut columns = ColumnWidths::default();
        columns.add_columns(Dimension::percent(0.14), 1);
        columns.add_columns(Dimension::auto(), 1);
        columns.first_row_cell(0, Dimension::length(50.0));
        columns.first_row_cell(1, Dimension::length(30.0));
        columns.first_row_cell(2, Dimension::length(20.0));
        assert_eq!(
            columns.into_tracks(3),
            [
                Dimension::percent(0.14),
                Dimension::length(30.0),
                Dimension::length(20.0)
            ]
        );
    }

    #[test]
    fn a_column_elements_span_sizes_as_many_columns() {
        let mut columns = ColumnWidths::default();
        columns.add_columns(Dimension::length(40.0), 2);
        for col in 0..3 {
            columns.first_row_cell(col, Dimension::auto());
        }
        assert_eq!(
            columns.into_tracks(3),
            [
                Dimension::length(40.0),
                Dimension::length(40.0),
                Dimension::auto()
            ]
        );
    }

    #[test]
    fn a_first_row_cell_sizes_the_column_it_starts_in() {
        let mut columns = ColumnWidths::default();
        columns.first_row_cell(0, Dimension::length(50.0));
        columns.first_row_cell(2, Dimension::length(30.0));
        assert_eq!(
            columns.into_tracks(3),
            [
                Dimension::length(50.0),
                Dimension::auto(),
                Dimension::length(30.0)
            ]
        );
    }

    #[test]
    fn a_cell_skips_the_columns_a_cell_of_an_earlier_row_spans_into() {
        let mut spans = RowSpans::default();
        spans.span(0, 1, 1..3, 2);
        assert_eq!(spans.free_column(2, 0), 0);
        assert_eq!(spans.free_column(2, 1), 3);
        assert_eq!(spans.free_column(3, 1), 1);
    }

    #[test]
    fn a_row_span_ends_with_its_row_group() {
        let mut items = vec![cell(), cell()];
        let mut spans = RowSpans::default();
        spans.span(0, 1, 0..1, 5);
        spans.span(1, 2, 1..2, 2);
        spans.end_group(&mut items, 3);
        assert_eq!(items[0].style.grid_row.end, style_helpers::span(3));
        assert_eq!(items[1].style.grid_row.end, style_helpers::span(2));
        assert_eq!(spans.free_column(4, 0), 0);
    }
}
