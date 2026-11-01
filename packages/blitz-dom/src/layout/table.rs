use std::{ops::Range, sync::Arc};

use markup5ever::local_name;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{Atom, computed_values::table_layout::T as TableLayout};
use taffy::{
    Dimension, LayoutPartialTree as _, MaxTrackSizingFunction, MinTrackSizingFunction,
    ResolveOrZero, TrackSizingFunction, compute_leaf_layout, style_helpers,
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

    let mut column_sizes = ColumnWidths::new(is_fixed);
    let mut row_spans = RowSpans::default();
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
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
        .into_tracks()
        .into_iter()
        .map(Into::into)
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
/// `colgroup` without `col` children) and that of the first-row cell
/// starting in it (CSS 2.1 §17.5.2).
#[derive(Debug)]
pub(crate) struct ColumnWidths {
    /// Whether the table's layout is fixed.
    fixed: bool,
    /// Each column's width, `auto` where none is set.
    widths: Vec<Dimension>,
    /// The most columns a row reaches.
    columns: u16,
}

impl ColumnWidths {
    fn new(fixed: bool) -> Self {
        Self {
            fixed,
            widths: Vec::new(),
            columns: 0,
        }
    }

    /// Adds the `span` columns of a column element whose `width` is `width`.
    fn add_columns(&mut self, width: Dimension, span: u16) {
        self.widths
            .extend(std::iter::repeat_n(width, usize::from(span)));
    }

    /// Sizes the column at `col` by a cell of the first row: in the fixed
    /// layout only where no column element sized it, in the automatic
    /// layout to the wider of the two.
    fn first_row_cell(&mut self, col: u16, width: Dimension) {
        let col = usize::from(col);
        if self.widths.len() <= col {
            self.widths.resize(col + 1, style_helpers::auto());
        }
        let column = &mut self.widths[col];
        *column = if self.fixed && !column.is_auto() {
            *column
        } else {
            wider(*column, width)
        };
    }

    /// Widens the column at `col` to a later row's cell `len` wide, in the
    /// automatic layout.
    fn later_row_cell(&mut self, col: u16, len: f32) {
        if self.fixed {
            return;
        }
        if let Some(column) = self.widths.get_mut(usize::from(col)) {
            *column = wider(*column, style_helpers::length(len));
        }
    }

    /// Records that a row reaches `columns` columns.
    fn end_row(&mut self, columns: u16) {
        self.columns = self.columns.max(columns);
    }

    /// The column tracks: one for each column a row or a column element
    /// makes. Percentages past 100% are cut from the last columns.
    ///
    /// In the automatic layout, when every column has a percentage, a
    /// column takes its share of the table and grows to its cells'
    /// min-content width, the others sharing the rest by their percentages:
    /// each track is `minmax(min-content, <percentage>fr)`, in percent
    /// since flex factors that sum below 1 leave space free. Otherwise each
    /// column has exactly its width. A track `minmax(min-content, <width>)`
    /// would grow past its width: Taffy 0.9's `distribute_space_up_to_limits`
    /// measures a track's room to its limit without the increase earlier
    /// rounds gave it.
    fn into_tracks(mut self) -> Vec<TrackSizingFunction> {
        let count = self.widths.len().max(usize::from(self.columns));
        self.widths.resize(count, style_helpers::auto());
        let mut percent_left = 1.0f32;
        for width in &mut self.widths {
            if width.tag() == taffy::CompactLength::PERCENT_TAG {
                let percent = width.value().min(percent_left);
                percent_left -= percent;
                *width = Dimension::percent(percent);
            }
        }
        let shares = !self.fixed
            && self
                .widths
                .iter()
                .all(|width| width.tag() == taffy::CompactLength::PERCENT_TAG);
        self.widths
            .into_iter()
            .map(|width| {
                if shares {
                    style_helpers::minmax(
                        MinTrackSizingFunction::min_content(),
                        MaxTrackSizingFunction::fr(width.value() * 100.0),
                    )
                } else {
                    TrackSizingFunction::from(width)
                }
            })
            .collect()
    }
}

/// The wider of two column widths; a percentage is wider than a length.
fn wider(a: Dimension, b: Dimension) -> Dimension {
    const AUTO: usize = taffy::CompactLength::AUTO_TAG;
    const LENGTH: usize = taffy::CompactLength::LENGTH_TAG;
    const PERCENT: usize = taffy::CompactLength::PERCENT_TAG;
    match (a.tag(), b.tag()) {
        (AUTO, _) | (LENGTH, PERCENT) => b,
        (LENGTH, LENGTH) | (PERCENT, PERCENT) if b.value() > a.value() => b,
        _ => a,
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
                collect_table_cells(doc, child_id, row, col, cells, columns, row_spans);
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
                collect_table_cells(doc, child_id, row, col, cells, columns, row_spans);
            }
            doc.nodes[node_id].children = children;
            // Count the columns that spans from above cover at the row's end.
            *col = row_spans.free_column(*row, *col);
            columns.end_row(*col);
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
            } else if taffy::CompactLength::LENGTH_TAG == style.size.width.tag() {
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

/// The `width` and `span` of a column element.
fn column_of(node: &Node) -> (Dimension, u16) {
    let width = node
        .primary_styles()
        .map_or_else(style_helpers::auto, |style| {
            stylo_taffy::convert::dimension(&style.get_position().width)
        });
    (width, node.attr(local_name!("span")).map_or(1, parse_span))
}

/// A column element's `span`: the HTML Standard's rules for parsing
/// non-negative integers (leading white space, a `+`, then the digits up to
/// the first other character), clamped to 1 to 1000; 1 where no digit
/// leads.
fn parse_span(value: &str) -> u16 {
    let value = value.trim_start_matches([' ', '\t', '\n', '\u{c}', '\r']);
    let value = value.strip_prefix('+').unwrap_or(value);
    let span = value
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0u32, |span, digit| {
            span.saturating_mul(10)
                .saturating_add(u32::from(digit - b'0'))
        });
    span.clamp(1, 1000) as u16
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
    use crate::{Attribute, DocumentConfig, DocumentMutator, LocalName, QualName, ns};

    /// An element: its name, its attributes, and its children.
    struct El(
        &'static str,
        &'static [(&'static str, &'static str)],
        Vec<El>,
    );

    /// A cell with no padding holding `children`.
    fn td(children: Vec<El>) -> El {
        El("td", &[("style", "padding:0")], children)
    }

    /// A row of `cells` empty cells.
    fn tr(cells: usize) -> El {
        El("tr", &[], (0..cells).map(|_| td(vec![])).collect())
    }

    fn append(mutator: &mut DocumentMutator, parent: usize, el: &El, cells: &mut Vec<usize>) {
        let attrs =
            el.1.iter()
                .map(|(name, value)| Attribute {
                    name: QualName::new(None, ns!(), LocalName::from(*name)),
                    value: value.to_string(),
                })
                .collect();
        let id =
            mutator.create_element(QualName::new(None, ns!(html), LocalName::from(el.0)), attrs);
        mutator.append_children(parent, &[id]);
        if el.0 == "td" {
            cells.push(id);
        }
        for child in &el.2 {
            append(mutator, id, child, cells);
        }
    }

    /// The x and width of each cell, in document order, of a 400 px wide
    /// table with no border spacing whose `col`s, `colgroup`s, and rows are
    /// `parts`.
    fn cells(table_layout: &'static str, parts: Vec<El>) -> Vec<(f32, f32)> {
        let style: &'static [(&str, &str)] = match table_layout {
            "fixed" => &[("style", "width:400px;border-spacing:0;table-layout:fixed")],
            _ => &[("style", "width:400px;border-spacing:0")],
        };
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(blitz_traits::shell::Viewport::new(
                800,
                600,
                1.0,
                blitz_traits::shell::ColorScheme::Light,
            )),
            ..Default::default()
        });
        let page = El(
            "html",
            &[],
            vec![El(
                "body",
                &[("style", "margin:0")],
                vec![El("table", style, parts)],
            )],
        );
        let mut ids = Vec::new();
        {
            let mut mutator = doc.mutate();
            append(&mut mutator, 0, &page, &mut ids);
        }
        doc.resolve(0.0);
        ids.iter()
            .map(|&id| {
                let layout = doc.nodes[id].final_layout;
                (layout.location.x, layout.size.width)
            })
            .collect()
    }

    fn col(style: &'static [(&'static str, &'static str)]) -> El {
        El("col", style, vec![])
    }

    #[test]
    fn a_cell_after_a_colspan_sizes_the_column_it_starts_in() {
        let first = El(
            "tr",
            &[],
            vec![
                El("td", &[("colspan", "2"), ("style", "padding:0")], vec![]),
                El("td", &[("style", "padding:0;width:100px")], vec![]),
            ],
        );
        let cells = cells("auto", vec![first, tr(3)]);
        assert_eq!(cells[4], (300.0, 100.0));
    }

    #[test]
    fn a_colgroup_without_cols_sizes_its_span_columns() {
        let group = El(
            "colgroup",
            &[("span", "2"), ("style", "width:50px")],
            vec![],
        );
        let cells = cells("fixed", vec![group, tr(3)]);
        assert_eq!(&cells[..2], [(0.0, 50.0), (50.0, 50.0)]);
    }

    #[test]
    fn a_col_span_is_read_as_html_reads_a_non_negative_integer() {
        let cols = col(&[("span", "3px"), ("style", "width:40px")]);
        let cells = cells("fixed", vec![cols, tr(4)]);
        assert_eq!(&cells[..3], [(0.0, 40.0), (40.0, 40.0), (80.0, 40.0)]);
    }

    #[test]
    fn a_narrow_col_grows_to_its_cells_min_content() {
        let wide = El("div", &[("style", "width:100px;height:10px")], vec![]);
        let row = El("tr", &[], vec![td(vec![wide]), td(vec![]), td(vec![])]);
        let cols = vec![
            col(&[("style", "width:5%")]),
            col(&[("style", "width:57%")]),
            col(&[("style", "width:38%")]),
        ];
        let cells = cells("auto", cols.into_iter().chain([row]).collect());
        assert_eq!(cells, [(0.0, 100.0), (100.0, 180.0), (280.0, 120.0)]);
    }

    #[test]
    fn a_short_last_row_keeps_the_columns_of_the_rows_before() {
        let cols = vec![
            col(&[("style", "width:20%")]),
            col(&[("style", "width:30%")]),
            col(&[("style", "width:50%")]),
        ];
        let cells = cells("auto", cols.into_iter().chain([tr(3), tr(1)]).collect());
        assert_eq!(&cells[..3], [(0.0, 80.0), (80.0, 120.0), (200.0, 200.0)]);
    }

    #[test]
    fn a_first_row_cell_wider_than_its_col_widens_the_column() {
        let row = El(
            "tr",
            &[],
            vec![
                El("td", &[("style", "padding:0;width:200px")], vec![]),
                td(vec![]),
            ],
        );
        let cells = cells("auto", vec![col(&[("style", "width:100px")]), row]);
        assert_eq!(cells, [(0.0, 200.0), (200.0, 200.0)]);
    }

    #[test]
    fn col_percentages_past_the_table_are_cut_from_the_last() {
        let cols = vec![
            col(&[("style", "width:60%")]),
            col(&[("style", "width:60%")]),
        ];
        let cells = cells("auto", cols.into_iter().chain([tr(2)]).collect());
        assert_eq!(cells, [(0.0, 240.0), (240.0, 160.0)]);
    }

    fn cell() -> TableItem {
        TableItem {
            kind: TableItemKind::Cell,
            node_id: 0,
            style: taffy::Style::default(),
        }
    }

    #[test]
    fn a_column_elements_width_comes_before_its_first_row_cells() {
        let mut columns = ColumnWidths::new(false);
        columns.add_columns(Dimension::percent(0.14), 1);
        columns.add_columns(Dimension::auto(), 1);
        columns.first_row_cell(0, Dimension::length(50.0));
        columns.first_row_cell(1, Dimension::length(30.0));
        columns.first_row_cell(2, Dimension::length(20.0));
        assert_eq!(
            columns.widths,
            [
                Dimension::percent(0.14),
                Dimension::length(30.0),
                Dimension::length(20.0)
            ]
        );
    }

    #[test]
    fn a_column_elements_span_sizes_as_many_columns() {
        let mut columns = ColumnWidths::new(false);
        columns.add_columns(Dimension::length(40.0), 2);
        for col in 0..3 {
            columns.first_row_cell(col, Dimension::auto());
        }
        assert_eq!(
            columns.widths,
            [
                Dimension::length(40.0),
                Dimension::length(40.0),
                Dimension::auto()
            ]
        );
    }

    #[test]
    fn a_first_row_cell_sizes_the_column_it_starts_in() {
        let mut columns = ColumnWidths::new(false);
        columns.first_row_cell(0, Dimension::length(50.0));
        columns.first_row_cell(2, Dimension::length(30.0));
        assert_eq!(
            columns.widths,
            [
                Dimension::length(50.0),
                Dimension::auto(),
                Dimension::length(30.0)
            ]
        );
    }

    #[test]
    fn a_first_row_cell_wider_than_its_col_widens_it_in_the_automatic_layout_only() {
        for (fixed, width) in [(false, 200.0), (true, 100.0)] {
            let mut columns = ColumnWidths::new(fixed);
            columns.add_columns(Dimension::length(100.0), 1);
            columns.first_row_cell(0, Dimension::length(200.0));
            assert_eq!(columns.widths, [Dimension::length(width)], "fixed: {fixed}");
        }
    }

    #[test]
    fn a_span_is_parsed_as_a_non_negative_integer_from_1_to_1000() {
        let spans = ["3", "3px", " +2", "70000", "0", "x", ""].map(parse_span);
        assert_eq!(spans, [3, 3, 2, 1000, 1, 1, 1]);
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
