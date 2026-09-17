use crate::helpers::configuration::Config;
use crate::ARROW_SCHEMA;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use datafusion::arrow::array::Array; // for is_null on Arrow arrays
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::{
    DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Axis, BarChart, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table,
    },
    Terminal,
};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use std::io::{stdout, Stdout};
use std::sync::mpsc;

// LiveTableView removed (unused)

fn build_rows(batches: &Vec<RecordBatch>) -> (Vec<String>, Vec<Vec<String>>) {
    if batches.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let schema = batches[0].schema();
    let headers: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for b in batches {
        let num_rows = b.num_rows();
        for r in 0..num_rows {
            let mut cols: Vec<String> = Vec::with_capacity(headers.len());
            for c in 0..b.num_columns() {
                let col = b.column(c);
                cols.push(value_to_string(col.as_ref(), r));
            }
            rows.push(cols);
        }
    }
    (headers, rows)
}

// Chart helpers: minimal heuristics to derive series from the first batch
fn build_bar_data(batches: &Vec<RecordBatch>) -> (Vec<String>, Vec<f64>) {
    if batches.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let b = &batches[0];
    if b.num_columns() == 0 {
        return (Vec::new(), Vec::new());
    }
    // Try to use first two columns: label(string-ish) + value(numeric)
    let mut labels: Vec<String> = Vec::new();
    let mut values: Vec<f64> = Vec::new();
    let cols = b.num_columns();
    for r in 0..b.num_rows() {
        let label = if matches!(b.column(0).data_type(), ArrowDataType::Utf8) {
            value_to_string(b.column(0).as_ref(), r)
        } else {
            r.to_string()
        };
        let mut val: Option<f64> = None;
        if cols > 1 {
            match b.column(1).data_type() {
                ArrowDataType::Int64 => {
                    if let Some(v) = b
                        .column(1)
                        .as_any()
                        .downcast_ref::<datafusion::arrow::array::Int64Array>()
                        .map(|a| a.value(r) as f64)
                    {
                        val = Some(v);
                    }
                }
                ArrowDataType::Float64 => {
                    if let Some(v) = b
                        .column(1)
                        .as_any()
                        .downcast_ref::<datafusion::arrow::array::Float64Array>()
                        .map(|a| a.value(r))
                    {
                        val = Some(v);
                    }
                }
                _ => {}
            }
        }
        labels.push(label);
        values.push(val.unwrap_or(0.0));
        if labels.len() >= 50 {
            break;
        }
    }
    (labels, values)
}

fn build_histogram_data(batches: &Vec<RecordBatch>, bins: usize) -> Vec<(f64, f64)> {
    if batches.is_empty() {
        return Vec::new();
    }
    let b = &batches[0];
    if b.num_columns() == 0 {
        return Vec::new();
    }
    // Use first numeric column
    let mut vals: Vec<f64> = Vec::new();
    for c in 0..b.num_columns() {
        match b.column(c).data_type() {
            ArrowDataType::Int64 => {
                let a = b
                    .column(c)
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                    .unwrap();
                for r in 0..a.len() {
                    if !a.is_null(r) {
                        vals.push(a.value(r) as f64);
                    }
                }
                break;
            }
            ArrowDataType::Float64 => {
                let a = b
                    .column(c)
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Float64Array>()
                    .unwrap();
                for r in 0..a.len() {
                    if !a.is_null(r) {
                        vals.push(a.value(r));
                    }
                }
                break;
            }
            _ => {}
        }
    }
    if vals.is_empty() {
        return Vec::new();
    }
    let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let width = if max > min {
        (max - min) / bins as f64
    } else {
        1.0
    };
    let mut counts = vec![0usize; bins];
    for v in vals {
        let mut idx = ((v - min) / width).floor() as isize;
        if idx < 0 {
            idx = 0;
        }
        if idx as usize >= bins {
            idx = bins as isize - 1;
        }
        counts[idx as usize] += 1;
    }
    let mut out: Vec<(f64, f64)> = Vec::new();
    for i in 0..bins {
        out.push((min + i as f64 * width, counts[i] as f64));
    }
    out
}

fn build_timeseries_data(batches: &Vec<RecordBatch>, max_points: usize) -> Vec<(f64, f64)> {
    if batches.is_empty() {
        return Vec::new();
    }
    let b = &batches[0];
    if b.num_columns() < 2 {
        return Vec::new();
    }
    // Heuristic: first timestamp-like column + first numeric column
    let mut ts_col: Option<usize> = None;
    let mut val_col: Option<usize> = None;
    for c in 0..b.num_columns() {
        match b.column(c).data_type() {
            ArrowDataType::Timestamp(_, _) => {
                if ts_col.is_none() {
                    ts_col = Some(c);
                }
            }
            ArrowDataType::Int64 | ArrowDataType::Float64 => {
                if val_col.is_none() {
                    val_col = Some(c);
                }
            }
            _ => {}
        }
    }
    let (tc, vc) = match (ts_col, val_col) {
        (Some(t), Some(v)) => (t, v),
        _ => return Vec::new(),
    };
    let mut pts: Vec<(f64, f64)> = Vec::new();
    for r in 0..b.num_rows() {
        // TimestampMillisecondArray is the common case
        let t_ms = b
            .column(tc)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::TimestampMillisecondArray>()
            .map(|a| a.value(r))
            .unwrap_or(0) as f64;
        let v = match b.column(vc).data_type() {
            ArrowDataType::Int64 => b
                .column(vc)
                .as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .map(|a| a.value(r) as f64)
                .unwrap_or(0.0),
            ArrowDataType::Float64 => b
                .column(vc)
                .as_any()
                .downcast_ref::<datafusion::arrow::array::Float64Array>()
                .map(|a| a.value(r))
                .unwrap_or(0.0),
            _ => 0.0,
        };
        pts.push((t_ms, v));
        if pts.len() >= max_points {
            break;
        }
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    pts
}

pub(crate) fn value_to_string(arr: &dyn datafusion::arrow::array::Array, row: usize) -> String {
    if arr.is_null(row) {
        return "".to_string();
    }
    use datafusion::arrow::array::*;
    use datafusion::arrow::datatypes::DataType as ArrowDt;
    match arr.data_type() {
        ArrowDt::Utf8 => arr
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        ArrowDt::Int64 => arr
            .as_any()
            .downcast_ref::<Int64Array>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        ArrowDt::Float64 => arr
            .as_any()
            .downcast_ref::<Float64Array>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        ArrowDt::Boolean => arr
            .as_any()
            .downcast_ref::<BooleanArray>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        ArrowDt::Timestamp(_, _) => arr
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .map(|a| format_ts_ms(a.value(row)))
            .unwrap_or_else(|| {
                // Fallback: use Debug for datatype if downcast failed
                format!("{:?}", arr)
            }),
        // Nested types → JSON serialize compactly
        ArrowDt::Struct(_)
        | ArrowDt::List(_)
        | ArrowDt::LargeList(_)
        | ArrowDt::FixedSizeList(_, _)
        | ArrowDt::Map(_, _) => {
            let v = array_cell_to_json(arr, row);
            match serde_json::to_string(&v) {
                Ok(s) => s,
                Err(_) => String::new(),
            }
        }
        // Other numeric primitives we commonly see
        ArrowDt::Int32 => arr
            .as_any()
            .downcast_ref::<Int32Array>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        ArrowDt::Float32 => arr
            .as_any()
            .downcast_ref::<Float32Array>()
            .map(|a| a.value(row).to_string())
            .unwrap_or_default(),
        // Fallback: debug
        _ => format!("{:?}", arr),
    }
}

pub(crate) fn array_cell_to_json(
    arr: &dyn datafusion::arrow::array::Array,
    row: usize,
) -> JsonValue {
    use datafusion::arrow::array::*;
    use datafusion::arrow::datatypes::DataType as ArrowDt;
    if arr.is_null(row) {
        return JsonValue::Null;
    }
    match arr.data_type() {
        ArrowDt::Utf8 => {
            if let Some(a) = arr.as_any().downcast_ref::<StringArray>() {
                JsonValue::String(a.value(row).to_string())
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Boolean => {
            if let Some(a) = arr.as_any().downcast_ref::<BooleanArray>() {
                JsonValue::Bool(a.value(row))
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Int64 => {
            if let Some(a) = arr.as_any().downcast_ref::<Int64Array>() {
                JsonValue::Number(JsonNumber::from(a.value(row)))
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Int32 => {
            if let Some(a) = arr.as_any().downcast_ref::<Int32Array>() {
                JsonValue::Number(JsonNumber::from(a.value(row)))
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Float64 => {
            if let Some(a) = arr.as_any().downcast_ref::<Float64Array>() {
                serde_json::Number::from_f64(a.value(row))
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Float32 => {
            if let Some(a) = arr.as_any().downcast_ref::<Float32Array>() {
                serde_json::Number::from_f64(a.value(row) as f64)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Timestamp(_, _) => {
            if let Some(a) = arr.as_any().downcast_ref::<TimestampMillisecondArray>() {
                JsonValue::String(format_ts_ms(a.value(row)))
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Struct(fields) => {
            if let Some(sa) = arr.as_any().downcast_ref::<StructArray>() {
                let mut map = JsonMap::new();
                for (i, fld) in fields.iter().enumerate() {
                    let name = fld.name();
                    let col = sa.column(i).as_ref();
                    let v = array_cell_to_json(col, row);
                    map.insert(name.clone(), v);
                }
                JsonValue::Object(map)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::List(_) => {
            if let Some(la) = arr.as_any().downcast_ref::<ListArray>() {
                let values = la.value(row);
                let mut out: Vec<JsonValue> = Vec::with_capacity(values.len());
                for i in 0..values.len() {
                    out.push(array_cell_to_json(values.as_ref(), i));
                }
                JsonValue::Array(out)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::LargeList(_) => {
            if let Some(lla) = arr.as_any().downcast_ref::<LargeListArray>() {
                let values = lla.value(row);
                let mut out: Vec<JsonValue> = Vec::with_capacity(values.len());
                for i in 0..values.len() {
                    out.push(array_cell_to_json(values.as_ref(), i));
                }
                JsonValue::Array(out)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::FixedSizeList(_, _) => {
            if let Some(fla) = arr.as_any().downcast_ref::<FixedSizeListArray>() {
                let values = fla.value(row);
                let mut out: Vec<JsonValue> = Vec::with_capacity(values.len());
                for i in 0..values.len() {
                    out.push(array_cell_to_json(values.as_ref(), i));
                }
                JsonValue::Array(out)
            } else {
                JsonValue::Null
            }
        }
        ArrowDt::Map(_, _) => {
            if let Some(ma) = arr.as_any().downcast_ref::<MapArray>() {
                // Represent as JSON object; Arrow map is a list of struct entries {key, value}
                let entries = ma.value(row);
                if let Some(entry_struct) = entries.as_any().downcast_ref::<StructArray>() {
                    let mut map = JsonMap::new();
                    if entry_struct.num_columns() >= 2 {
                        let keys_arr = entry_struct.column(0).as_ref();
                        let vals_arr = entry_struct.column(1).as_ref();
                        for i in 0..entry_struct.len() {
                            let k_json = array_cell_to_json(keys_arr, i);
                            let key_str = match k_json {
                                JsonValue::String(s) => s,
                                other => match serde_json::to_string(&other) {
                                    Ok(s) => s,
                                    Err(_) => String::new(),
                                },
                            };
                            let v_json = array_cell_to_json(vals_arr, i);
                            map.insert(key_str, v_json);
                        }
                    }
                    JsonValue::Object(map)
                } else {
                    JsonValue::Null
                }
            } else {
                JsonValue::Null
            }
        }
        // Fallback: best-effort string using Debug
        _ => JsonValue::String(format!("{:?}", arr)),
    }
}

fn format_ts_ms(v: i64) -> String {
    if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(v) {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0) {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        String::new()
    }
}

pub struct QueryEditorConfig<'a> {
    pub title: &'a str,
    pub footer: Option<&'a str>,
    #[allow(dead_code)]
    pub initial_sql: &'a str,
}

pub struct QueryEditorView {
    input: String,
    cursor: usize,
    rows_offset: usize,
    sugg_idx: usize,
    mode: ViewMode,
    menu_active: bool,
    menu_idx: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Table,
    BarV,
    BarH,
    Histogram,
    TimeSeries,
}

fn view_modes_labels() -> Vec<&'static str> {
    vec!["Table", "BarV", "BarH", "Hist", "Series"]
}
fn mode_to_index(m: ViewMode) -> usize {
    match m {
        ViewMode::Table => 0,
        ViewMode::BarV => 1,
        ViewMode::BarH => 2,
        ViewMode::Histogram => 3,
        ViewMode::TimeSeries => 4,
    }
}
fn index_to_mode(i: usize) -> ViewMode {
    match i {
        0 => ViewMode::Table,
        1 => ViewMode::BarV,
        2 => ViewMode::BarH,
        3 => ViewMode::Histogram,
        4 => ViewMode::TimeSeries,
        _ => ViewMode::Table,
    }
}

impl QueryEditorView {
    pub fn new(initial: &str) -> Self {
        Self {
            input: initial.to_string(),
            cursor: initial.len(),
            rows_offset: 0,
            sugg_idx: 0,
            mode: ViewMode::Table,
            menu_active: false,
            menu_idx: 0,
        }
    }

    pub fn run(
        self,
        config: &Config,
        cfg: QueryEditorConfig,
        result_rx: mpsc::Receiver<Vec<RecordBatch>>,
        request_tx: mpsc::Sender<String>,
    ) {
        let mut s = self;
        let mut out: Stdout = stdout();
        let _ = enable_raw_mode();
        let _ = execute!(out, EnterAlternateScreen);
        let backend = CrosstermBackend::new(out);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut data: Vec<RecordBatch> = Vec::new();

        loop {
            // receive new results if available
            if let Ok(b) = result_rx.try_recv() {
                data = b;
                s.rows_offset = 0;
            }
            let (headers, rows) = build_rows(&data);
            let title = cfg.title;
            let footer_text = cfg.footer.unwrap_or("");

            // suggestions
            let (tok, tok_start) = current_token(&s.input, s.cursor);
            // Prefer ARROW_SCHEMA for the table if available; otherwise fall back to current result schema
            let schema_arc_opt = if !data.is_empty() {
                Some(data[0].schema())
            } else {
                None
            };
            let current_schema_opt: Option<&ArrowSchema> = schema_arc_opt.as_deref();
            let suggestions =
                build_suggestions(config, &s.input, tok, current_schema_opt, s.cursor, &data);
            if s.sugg_idx >= suggestions.len() {
                s.sugg_idx = 0;
            }

            let _ = terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Length(1),
                            Constraint::Min(1),
                            Constraint::Length(3),
                            Constraint::Length(1),
                        ]
                        .as_ref(),
                    )
                    .split(f.size());

                let header = Paragraph::new(Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                f.render_widget(header, chunks[0]);

                // results area: table or charts
                match s.mode {
                    ViewMode::Table => {
                        let table = if headers.is_empty() {
                            Table::new(Vec::<Row>::new(), vec![Constraint::Percentage(100)])
                                .block(Block::default().borders(Borders::ALL).title("results"))
                        } else {
                            let widths: Vec<Constraint> =
                                headers.iter().map(|_| Constraint::Min(6)).collect();
                            let header_row =
                                Row::new(headers.iter().map(|h| Cell::from(h.clone())))
                                    .style(Style::default().add_modifier(Modifier::BOLD));
                            let body_rows_iter = rows
                                .iter()
                                .skip(s.rows_offset)
                                .map(|r| Row::new(r.iter().map(|c| Cell::from(c.clone()))));
                            let mut body_rows: Vec<Row> = Vec::new();
                            for r in body_rows_iter {
                                body_rows.push(r);
                                if body_rows.len() > chunks[1].height.saturating_sub(3) as usize {
                                    break;
                                }
                            }
                            Table::new(body_rows, widths)
                                .header(header_row)
                                .block(Block::default().borders(Borders::ALL).title("results"))
                        };
                        f.render_widget(table, chunks[1]);
                    }
                    ViewMode::BarV | ViewMode::BarH => {
                        let (labels, values) = build_bar_data(&data);
                        let mut items: Vec<(&str, u64)> = Vec::new();
                        let owned: Vec<String> = labels;
                        for (i, v) in values.iter().enumerate() {
                            if i < owned.len() {
                                items.push((owned[i].as_str(), (*v) as u64));
                            }
                        }
                        let mut bar = BarChart::default()
                            .block(Block::default().borders(Borders::ALL).title("bar"))
                            .data(&items);
                        if s.mode == ViewMode::BarH {
                            bar = bar.direction(Direction::Horizontal);
                        }
                        f.render_widget(bar, chunks[1]);
                    }
                    ViewMode::Histogram => {
                        let pts = build_histogram_data(&data, 20);
                        let dataset = Dataset::default()
                            .name("hist")
                            .marker(ratatui::symbols::Marker::Braille)
                            .graph_type(GraphType::Line)
                            .style(Style::default().fg(Color::Cyan))
                            .data(&pts);
                        let x_bounds = if pts.is_empty() {
                            [0.0, 1.0]
                        } else {
                            [pts.first().unwrap().0, pts.last().unwrap().0]
                        };
                        let y_max = pts.iter().map(|p| p.1).fold(0.0, f64::max).max(1.0);
                        let chart = Chart::new(vec![dataset])
                            .block(Block::default().borders(Borders::ALL).title("histogram"))
                            .x_axis(Axis::default().bounds(x_bounds))
                            .y_axis(Axis::default().bounds([0.0, y_max]));
                        f.render_widget(chart, chunks[1]);
                    }
                    ViewMode::TimeSeries => {
                        let pts = build_timeseries_data(&data, 60);
                        let dataset = Dataset::default()
                            .name("ts")
                            .graph_type(GraphType::Line)
                            .style(Style::default().fg(Color::Green))
                            .data(&pts);
                        let x_bounds = if pts.is_empty() {
                            [0.0, 1.0]
                        } else {
                            [pts.first().unwrap().0, pts.last().unwrap().0]
                        };
                        let y_max = pts.iter().map(|p| p.1).fold(0.0, f64::max).max(1.0);
                        let chart = Chart::new(vec![dataset])
                            .block(Block::default().borders(Borders::ALL).title("timeseries"))
                            .x_axis(Axis::default().bounds(x_bounds))
                            .y_axis(Axis::default().bounds([0.0, y_max]));
                        f.render_widget(chart, chunks[1]);
                    }
                }

                // input area with inline ghost suggestion
                let input_block = Block::default().borders(Borders::ALL).title("sql");
                let mut segs: Vec<Span> = Vec::new();
                let before = &s.input[..tok_start];
                let typed = &s.input[tok_start..s.cursor];
                segs.push(Span::raw(before));
                segs.push(Span::raw(typed));
                if let Some(sg) = suggestions.get(s.sugg_idx) {
                    if !tok.is_empty() {
                        // Determine if cursor is at end of the current word
                        let mut tok_end = s.cursor;
                        let bytes = s.input.as_bytes();
                        while tok_end < s.input.len() {
                            let ch = bytes[tok_end] as char;
                            if !is_word_char(ch) {
                                break;
                            }
                            tok_end += 1;
                        }
                        let full_token = &s.input[tok_start..tok_end];
                        // If cursor is inside a word, or full word already equals a suggestion, do not ghost
                        let equals_suggestion = full_token.eq_ignore_ascii_case(sg);
                        let at_word_end = s.cursor == tok_end;
                        if at_word_end && !equals_suggestion {
                            // compute suffix in a case-aware manner
                            let mut candidate = sg.clone();
                            // simple case harmonization: if user typed all-caps, show suffix caps; if all-lower, keep lower
                            if tok == tok.to_uppercase() {
                                candidate = candidate.to_uppercase();
                            } else if tok == tok.to_lowercase() {
                                candidate = candidate.to_lowercase();
                            }
                            // Only show if the candidate starts with what is typed so far
                            if candidate.to_lowercase().starts_with(&tok.to_lowercase())
                                && candidate.len() > tok.len()
                            {
                                let suffix = &candidate[tok.len()..];
                                segs.push(Span::styled(
                                    suffix.to_string(),
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                        }
                    }
                }
                let after = &s.input[s.cursor..];
                segs.push(Span::raw(after));
                let inp = Paragraph::new(Line::from(segs)).block(input_block);
                f.render_widget(inp, chunks[2]);

                // footer + suggestions or menu
                let mut foot = footer_text.to_string();
                if s.menu_active {
                    let modes = view_modes_labels();
                    let mut items: Vec<String> = Vec::new();
                    for (i, label) in modes.iter().enumerate() {
                        if i == s.menu_idx {
                            items.push(format!("[{}]", label));
                        } else {
                            items.push(label.to_string());
                        }
                    }
                    if !foot.is_empty() {
                        foot.push_str(" | ");
                    }
                    foot.push_str(&format!("views: {}", items.join("  ")));
                    foot.push_str(" | Esc/F10: back to SQL");
                } else {
                    if !suggestions.is_empty() {
                        let max = 5usize;
                        let start = if s.sugg_idx >= max {
                            s.sugg_idx - (max - 1)
                        } else {
                            0
                        };
                        let end = std::cmp::min(start + max, suggestions.len());
                        let mut items: Vec<String> = Vec::new();
                        for (i, sg) in suggestions.iter().enumerate().take(end).skip(start) {
                            if i == s.sugg_idx {
                                items.push(format!("[{}]", sg));
                            } else {
                                items.push(sg.clone());
                            }
                        }
                        if !foot.is_empty() {
                            foot.push_str(" | ");
                        }
                        foot.push_str(&format!("suggest: {}", items.join("  ")));
                    }
                    // controls hint
                    if !foot.is_empty() {
                        foot.push_str(" | ");
                    }
                    foot.push_str("menu: F2 | run: Enter/Cmd+R | quit: Esc");
                }
                let footer = Paragraph::new(Span::raw(foot));
                f.render_widget(footer, chunks[3]);

                // cursor
                let cur = input_cursor_pos(chunks[2], s.cursor);
                f.set_cursor(cur.0, cur.1);
            });

            if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
                match event::read().unwrap() {
                    Event::Key(k) => match k.code {
                        KeyCode::Esc => {
                            if s.menu_active {
                                s.menu_active = false;
                            } else {
                                break;
                            }
                        }
                        // Command+R (SUPER+R on mac) to run; Enter also runs
                        KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::SUPER) => {
                            let _ = request_tx.send(s.input.clone());
                        }
                        KeyCode::Enter => {
                            let _ = request_tx.send(s.input.clone());
                        }
                        // Open menu (F2)
                        KeyCode::F(2) => {
                            s.menu_active = true;
                            s.menu_idx = mode_to_index(s.mode);
                        }
                        // Close menu (F10)
                        KeyCode::F(10) => {
                            s.menu_active = false;
                        }
                        // Menu navigation
                        KeyCode::Up if s.menu_active => {
                            let modes_len = view_modes_labels().len();
                            if modes_len > 0 {
                                s.menu_idx = (s.menu_idx + modes_len - 1) % modes_len;
                                s.mode = index_to_mode(s.menu_idx);
                            }
                        }
                        KeyCode::Down if s.menu_active => {
                            let modes_len = view_modes_labels().len();
                            if modes_len > 0 {
                                s.menu_idx = (s.menu_idx + 1) % modes_len;
                                s.mode = index_to_mode(s.menu_idx);
                            }
                        }
                        // When menu is open, ignore other input edits
                        key if s.menu_active => {
                            let _ = key;
                        }
                        // Option+Tab (Alt+Tab) to move to next word; Shift+Alt+Tab to previous word
                        KeyCode::Tab
                            if k.modifiers.contains(KeyModifiers::ALT)
                                && k.modifiers.contains(KeyModifiers::SHIFT) =>
                        {
                            s.cursor = prev_word(&s.input, s.cursor);
                        }
                        KeyCode::Tab if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.cursor = next_word(&s.input, s.cursor);
                        }
                        // Tab (no modifier) to accept current suggestion
                        KeyCode::Tab => {
                            if !suggestions.is_empty() {
                                let replacement = suggestions.get(s.sugg_idx).unwrap().clone();
                                let mut new_input = s.input.clone();
                                new_input.replace_range(tok_start..s.cursor, &replacement);
                                s.input = new_input;
                                s.cursor = tok_start + replacement.len();
                            }
                        }
                        // Alt+Up/Down to cycle suggestions
                        KeyCode::Up if k.modifiers.contains(KeyModifiers::ALT) => {
                            if s.sugg_idx > 0 {
                                s.sugg_idx -= 1;
                            }
                        }
                        KeyCode::Down if k.modifiers.contains(KeyModifiers::ALT) => {
                            if !suggestions.is_empty() {
                                s.sugg_idx = (s.sugg_idx + 1) % suggestions.len();
                            }
                        }
                        // Ctrl+N / Ctrl+P to cycle suggestions (like readline/zsh)
                        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !suggestions.is_empty() {
                                s.sugg_idx = (s.sugg_idx + 1) % suggestions.len();
                            }
                        }
                        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            if s.sugg_idx > 0 {
                                s.sugg_idx -= 1;
                            }
                        }
                        // View mode toggles (legacy direct bindings still supported with Option)
                        KeyCode::Char('t') if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.mode = ViewMode::Table;
                        }
                        KeyCode::Char('v') if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.mode = ViewMode::BarV;
                        }
                        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.mode = ViewMode::BarH;
                        }
                        KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.mode = ViewMode::Histogram;
                        }
                        KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::ALT) => {
                            s.mode = ViewMode::TimeSeries;
                        }
                        KeyCode::Left => {
                            if s.cursor > 0 {
                                s.cursor = prev_char_boundary(&s.input, s.cursor);
                            }
                        }
                        KeyCode::Right => {
                            if s.cursor < s.input.len() {
                                s.cursor = next_char_boundary(&s.input, s.cursor);
                            }
                        }
                        KeyCode::Home => {
                            s.cursor = 0;
                        }
                        KeyCode::End => {
                            s.cursor = s.input.len();
                        }
                        KeyCode::Backspace => {
                            if s.cursor > 0 {
                                let prev = prev_char_boundary(&s.input, s.cursor);
                                let _ = s.input.drain(prev..s.cursor);
                                s.cursor = prev;
                            }
                        }
                        KeyCode::Delete => {
                            if s.cursor < s.input.len() {
                                let next = next_char_boundary(&s.input, s.cursor);
                                let _ = s.input.drain(s.cursor..next);
                            }
                        }
                        KeyCode::Up => {
                            s.rows_offset = s.rows_offset.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            s.rows_offset = s.rows_offset.saturating_add(1);
                        }
                        KeyCode::PageUp => {
                            s.rows_offset = s.rows_offset.saturating_sub(20);
                        }
                        KeyCode::PageDown => {
                            s.rows_offset = s.rows_offset.saturating_add(20);
                        }
                        KeyCode::Char(c) => {
                            s.input.insert(s.cursor, c);
                            s.cursor = s.cursor.saturating_add(c.len_utf8());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }

        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();
    }
}

fn input_cursor_pos(area: Rect, cursor: usize) -> (u16, u16) {
    // Convert byte index to grapheme count for x position to avoid drift on multi-byte chars
    let x_offset = s_len_chars(&""[..]) as u16; // placeholder 0
    (
        area.x
            .saturating_add(1 + (cursor as u16).saturating_sub(x_offset)),
        area.y.saturating_add(1),
    )
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn s_len_chars(s: &str) -> usize {
    s.chars().count()
}

fn next_word(input: &str, cursor: usize) -> usize {
    let mut i = cursor;
    while i < input.len() {
        let nb = next_char_boundary(input, i);
        let ch = input[i..nb].chars().next().unwrap_or('\0');
        i = nb;
        if is_word_char(ch) {
            break;
        }
    }
    while i < input.len() {
        let nb = next_char_boundary(input, i);
        let ch = input[i..nb].chars().next().unwrap_or('\0');
        if !is_word_char(ch) {
            break;
        }
        i = nb;
    }
    i
}

fn prev_word(input: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor;
    while i > 0 {
        let pb = prev_char_boundary(input, i);
        let ch = input[pb..i].chars().next().unwrap_or('\0');
        i = pb;
        if is_word_char(ch) {
            break;
        }
    }
    while i > 0 {
        let pb = prev_char_boundary(input, i);
        let ch = input[pb..i].chars().next().unwrap_or('\0');
        if !is_word_char(ch) {
            break;
        }
        i = pb;
    }
    i
}

fn current_token<'a>(input: &'a str, cursor: usize) -> (&'a str, usize) {
    let mut start = cursor;
    while start > 0 {
        let cb = prev_char_boundary(input, start);
        let c = input[cb..start].chars().next().unwrap_or('\0');
        if is_word_char(c) {
            start = cb;
        } else {
            break;
        }
    }
    (&input[start..cursor], start)
}

fn build_suggestions(
    config: &Config,
    input: &str,
    prefix: &str,
    current_schema: Option<&ArrowSchema>,
    cursor: usize,
    batches: &Vec<RecordBatch>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let has_prefix = !prefix.is_empty();
    // Determine simple context around cursor
    let is_projection_ctx = is_in_select_list(input, cursor);
    let up_to_cursor = input[..cursor].to_uppercase();
    let from_pos = up_to_cursor.rfind(" FROM ");
    let where_pos = up_to_cursor.rfind(" WHERE ");
    let order_pos = up_to_cursor.rfind(" ORDER BY ");
    let group_pos = up_to_cursor.rfind(" GROUP BY ");
    let limit_pos = up_to_cursor.rfind(" LIMIT ");
    let after_from_ctx = match from_pos {
        Some(fp) => {
            let blockers = [where_pos, order_pos, group_pos, limit_pos];
            blockers.iter().all(|p| p.map_or(true, |pp| pp < fp))
        }
        None => false,
    };
    let in_limit_ctx = limit_pos.map_or(false, |lp| Some(lp) > from_pos);
    let in_where_ctx = where_pos.map_or(false, |wp| Some(wp) > from_pos);
    let in_where_value_ctx =
        in_where_ctx && is_where_value_position(input, cursor, where_pos.unwrap());
    let in_order_ctx = order_pos.map_or(false, |op| Some(op) > from_pos);
    let in_group_ctx = group_pos.map_or(false, |gp| Some(gp) > from_pos);

    let mut kw: Vec<&str> = Vec::new();
    if after_from_ctx {
        kw.extend(
            [
                "where", "group", "order", "limit", "join", "left", "inner", "right", "full",
            ]
            .iter()
            .copied(),
        );
    } else if in_where_ctx {
        kw.extend(
            ["and", "or", "not", "group", "order", "limit"]
                .iter()
                .copied(),
        );
    } else if in_order_ctx || in_group_ctx {
        // prefer fields; minimal extra keywords
        kw.extend(["asc", "desc"].iter().copied());
    } else if is_projection_ctx {
        kw.extend(["as"].iter().copied());
    } else {
        kw.extend(
            [
                "select", "from", "where", "group", "order", "limit", "stream", "window", "and",
                "or", "not", "as",
            ]
            .iter()
            .copied(),
        );
    }
    let udfs_base = [
        "lateness",
        "new_session",
        "zscore",
        "is_outlier_z",
        "to_timestamp_millis",
    ];
    let udfs: Vec<&str> = udfs_base
        .into_iter()
        .chain(
            crate::sqlrt::udfs::observability_udf_names()
                .iter()
                .copied(),
        )
        .collect();
    for k in kw.iter() {
        if !has_prefix || k.starts_with(&prefix.to_lowercase()) {
            out.push(k.to_string());
        }
    }
    // UDFs relevant mainly in projection/where/order/group contexts
    if is_projection_ctx || in_where_ctx || in_order_ctx || in_group_ctx {
        for u in udfs.iter() {
            if u.starts_with(&prefix.to_lowercase()) {
                out.push(u.to_string());
            }
        }
    }
    // table names from config
    if after_from_ctx || (!is_projection_ctx && !in_where_ctx && !in_order_ctx && !in_group_ctx) {
        // Prefer tables only when appropriate (e.g., after FROM or at top-level)
        for t in config.get_pipelines() {
            if !has_prefix || t.starts_with(prefix) {
                out.push(t);
            }
        }
    }
    // LIMIT numeric suggestions
    if in_limit_ctx {
        let nums = ["10", "100", "1000", "5000", "10000"]; // common limits
        for n in nums.iter() {
            if !has_prefix || n.starts_with(prefix) {
                out.push(n.to_string());
            }
        }
    }
    // fields from ARROW_SCHEMA — only in relevant contexts
    let (table_opt, alias_opt) = extract_table_and_alias(input);
    if is_projection_ctx || (in_where_ctx && !in_where_value_ctx) || in_order_ctx || in_group_ctx {
        if let Some(table) = table_opt {
            if let Some(entry) = ARROW_SCHEMA.get(&table) {
                let schema = entry.value().load();
                let mut fields: Vec<String> = Vec::new();
                flatten_fields_old(schema.as_ref(), None, &mut fields);
                for f in fields.iter() {
                    if !has_prefix || f.starts_with(prefix) {
                        out.push(f.clone());
                    }
                }
                if let Some(alias) = alias_opt.as_deref() {
                    for f in fields {
                        let aliased = format!("{}.{}", alias, f);
                        if !has_prefix || aliased.starts_with(prefix) {
                            out.push(aliased);
                        }
                    }
                }
            } else if let Some(schema) = current_schema {
                // fallback: use current result schema
                let mut fields: Vec<String> = Vec::new();
                flatten_fields(schema, None, &mut fields);
                for f in fields.iter() {
                    if !has_prefix || f.starts_with(prefix) {
                        out.push(f.clone());
                    }
                }
                if let Some(alias) = alias_opt.as_deref() {
                    for f in fields {
                        let aliased = format!("{}.{}", alias, f);
                        if !has_prefix || aliased.starts_with(prefix) {
                            out.push(aliased);
                        }
                    }
                }
            }
        } else if let Some(schema) = current_schema {
            // no table parsed yet; still suggest from current schema
            let mut fields: Vec<String> = Vec::new();
            flatten_fields(schema, None, &mut fields);
            for f in fields {
                if !has_prefix || f.starts_with(prefix) {
                    out.push(f);
                }
            }
        }
    }
    // WHERE value suggestions: sample distinct values from current data
    if in_where_value_ctx {
        if let Some((_table_opt2, alias_opt2)) = extract_table_and_alias(input).into() {
            if let Some(lhs_field) =
                extract_lhs_field_in_where(input, cursor, where_pos.unwrap_or(0))
            {
                let target = match alias_opt2.as_deref() {
                    Some(alias) if lhs_field.starts_with(&format!("{}.", alias)) => {
                        lhs_field[alias.len() + 1..].to_string()
                    }
                    _ => lhs_field.clone(),
                };
                if let Some(b) = batches.get(0) {
                    let schema = b.schema();
                    if let Some((col_idx, _)) = schema
                        .fields()
                        .iter()
                        .enumerate()
                        .find(|(_, f)| f.name() == &target)
                    {
                        use std::collections::HashSet;
                        let mut seen: HashSet<String> = HashSet::new();
                        let arr = b.column(col_idx).as_ref();
                        for r in 0..b.num_rows() {
                            let v = value_to_string(arr, r);
                            if v.is_empty() {
                                continue;
                            }
                            if !has_prefix || v.to_lowercase().starts_with(&prefix.to_lowercase()) {
                                if seen.insert(v.clone()) {
                                    out.push(v);
                                }
                                if seen.len() >= 30 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_where_value_position(input: &str, cursor: usize, where_pos_idx: usize) -> bool {
    // Heuristic: in the substring after WHERE, if the last significant token is an operator, assume value position
    let segment = input[where_pos_idx..cursor].to_uppercase();
    // Identify last operator occurrence
    let ops = [
        " = ",
        " != ",
        " <> ",
        " >= ",
        " <= ",
        " > ",
        " < ",
        " IN ",
        " LIKE ",
        " BETWEEN ",
    ];
    let mut last_op: Option<usize> = None;
    for op in ops.iter() {
        if let Some(idx) = segment.rfind(op) {
            last_op = Some(last_op.map_or(idx, |cur| cur.max(idx)));
        }
    }
    if last_op.is_none() {
        return false;
    }
    let last_op_pos = last_op.unwrap();
    // Any boundary keywords after operator? then not value position
    let bounds = [" AND ", " OR ", " GROUP BY ", " ORDER BY ", " LIMIT "];
    let mut last_bound: Option<usize> = None;
    for b in bounds.iter() {
        if let Some(idx) = segment.rfind(b) {
            last_bound = Some(last_bound.map_or(idx, |cur| cur.max(idx)));
        }
    }
    match last_bound {
        Some(b) => last_op_pos > b,
        None => true,
    }
}

fn extract_lhs_field_in_where(input: &str, cursor: usize, where_pos_idx: usize) -> Option<String> {
    let segment = &input[where_pos_idx..cursor];
    // find last operator position
    let ops = [
        "=",
        "!=",
        "<>",
        ">=",
        "<=",
        ">",
        "<",
        " in ",
        " like ",
        " between ",
    ];
    let seg_low = segment.to_lowercase();
    let mut last: Option<usize> = None;
    for op in ops.iter() {
        if let Some(idx) = seg_low.rfind(op) {
            last = Some(last.map_or(idx, |cur| cur.max(idx)));
        }
    }
    let pos = last?;
    // scan left for a field token ending at pos
    let bytes = segment.as_bytes();
    let mut i = pos;
    if i == 0 {
        return None;
    }
    i = i.saturating_sub(1);
    // skip spaces
    while i > 0 && bytes[i].is_ascii_whitespace() {
        i = i.saturating_sub(1);
    }
    // collect word chars (., _, alnum)
    let mut start = i;
    while start > 0 {
        let ch = bytes[start] as char;
        if ch.is_alphanumeric() || ch == '_' || ch == '.' {
            start = start.saturating_sub(1);
        } else {
            break;
        }
    }
    // adjust start forward to first char
    let begin = if start == 0 { 0 } else { start + 1 };
    let end = i + 1;
    if end <= begin {
        return None;
    }
    Some(segment[begin..end].trim().to_string())
}

fn flatten_fields(schema: &ArrowSchema, prefix: Option<String>, out: &mut Vec<String>) {
    for f in schema.fields() {
        flatten_field(f, prefix.as_deref(), out);
    }
}

fn flatten_field(field: &ArrowField, parent: Option<&str>, out: &mut Vec<String>) {
    let name = if let Some(p) = parent {
        format!("{}.{}", p, field.name())
    } else {
        field.name().to_string()
    };
    match field.data_type() {
        ArrowDataType::Struct(fields) => {
            out.push(name.clone());
            for ch in fields {
                flatten_field(ch, Some(&name), out);
            }
        }
        ArrowDataType::List(elem) => {
            // list element
            let elem_field = elem.as_ref();
            flatten_field(elem_field, Some(&name), out);
        }
        _ => {
            out.push(name);
        }
    }
}

// Compatibility: ARROW_SCHEMA stores arrow_schema::Schema; provide a parallel flattener
fn flatten_fields_old(
    schema: &arrow_schema::Schema,
    prefix: Option<String>,
    out: &mut Vec<String>,
) {
    for f in schema.fields() {
        flatten_field_old(f, prefix.as_deref(), out);
    }
}

fn flatten_field_old(field: &arrow_schema::Field, parent: Option<&str>, out: &mut Vec<String>) {
    let name = if let Some(p) = parent {
        format!("{}.{}", p, field.name())
    } else {
        field.name().to_string()
    };
    match field.data_type() {
        arrow_schema::DataType::Struct(fields) => {
            out.push(name.clone());
            for ch in fields {
                flatten_field_old(ch, Some(&name), out);
            }
        }
        arrow_schema::DataType::List(elem) => {
            let elem_field = elem.as_ref();
            flatten_field_old(elem_field, Some(&name), out);
        }
        _ => {
            out.push(name);
        }
    }
}

#[allow(dead_code)]
fn extract_table_name(sql: &str) -> Option<String> {
    let up = sql.to_uppercase();
    if let Some(i) = up.find(" FROM ") {
        let rest = &sql[i + 6..];
        let mut name = String::new();
        for ch in rest.chars() {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                name.push(ch);
            } else {
                break;
            }
        }
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn extract_table_and_alias(sql: &str) -> (Option<String>, Option<String>) {
    let up = sql.to_uppercase();
    if let Some(i) = up.find(" FROM ") {
        let rest = &sql[i + 6..];
        // tokenize rest: table [AS] alias
        let mut it = rest.split_whitespace();
        if let Some(table_tok) = it.next() {
            // strip punctuation
            let table_name: String = table_tok
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '-')
                .collect();
            if table_name.is_empty() {
                return (None, None);
            }
            let mut alias_opt: Option<String> = None;
            if let Some(next_tok) = it.next() {
                let next_up = next_tok.to_uppercase();
                if next_up == "AS" {
                    if let Some(alias_tok) = it.next() {
                        alias_opt = Some(
                            alias_tok
                                .chars()
                                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                                .collect(),
                        );
                    }
                } else {
                    // treat immediate token as alias if not a keyword
                    let is_kw = matches!(
                        next_up.as_str(),
                        "WHERE"
                            | "GROUP"
                            | "ORDER"
                            | "LIMIT"
                            | "JOIN"
                            | "INNER"
                            | "LEFT"
                            | "RIGHT"
                            | "FULL"
                            | "ON"
                    );
                    if !is_kw {
                        alias_opt = Some(
                            next_tok
                                .chars()
                                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                                .collect(),
                        );
                    }
                }
            }
            return (Some(table_name), alias_opt.filter(|s| !s.is_empty()));
        }
    }
    (None, None)
}

fn is_in_select_list(input: &str, cursor: usize) -> bool {
    // Heuristic: if "select" appears before cursor and a "from" appears after the current token boundary
    let up = input[..cursor].to_uppercase();
    if let Some(sel_idx) = up.rfind("SELECT") {
        // ensure no FROM between SELECT and cursor
        let after_cursor = input[cursor..].to_uppercase();
        if after_cursor.contains(" FROM ") || up[sel_idx..].contains(" FROM ") == false {
            // If FROM not yet encountered before cursor, we are likely in projection list
            // Also avoid adding after a '*' token
            // Check previous non-space char is not '*'
            let mut i = cursor;
            let bytes = input.as_bytes();
            while i > 0 && bytes[i - 1].is_ascii_whitespace() {
                i -= 1;
            }
            if i > 0 && bytes[i - 1] as char == '*' {
                return false;
            }
            return true;
        }
    }
    false
}
