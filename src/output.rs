//! Result tables as TSV (the default), JSON lines or CSV. Nothing is cut unless `--max-cell`
//! asks for it, and a cut is marked with what was removed.

use std::io::{self, Write};

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Format {
    // No per-value help: with it, clap prints every `--help` in its long, double-spaced layout.
    Tsv,
    Json,
    Csv,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    /// The PostgreSQL type as the server reports it (`numeric`, `bytea`, `int8`, …), or empty.
    pub ty: String,
}

impl Column {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), ty: String::new() }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn new(names: &[&str]) -> Self {
        Self { columns: names.iter().map(|n| Column::new(n)).collect(), rows: Vec::new() }
    }

    /// Rows as the API returns them, one object per row. Without `columns` (the query was not
    /// compiled first) the first row's keys give the order.
    pub fn from_objects(mut columns: Vec<Column>, objects: Vec<Value>) -> Self {
        if columns.is_empty()
            && let Some(Value::Object(first)) = objects.first()
        {
            columns = first.keys().map(|k| Column::new(k)).collect();
        }
        let rows = objects
            .into_iter()
            .map(|object| {
                let mut object = match object {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                columns.iter().map(|c| object.remove(&c.name).unwrap_or(Value::Null)).collect()
            })
            .collect();
        Self { columns, rows }
    }

    pub fn write(&self, format: Format, max_cell: usize, out: &mut impl Write) -> io::Result<()> {
        match format {
            Format::Tsv => {
                writeln!(out, "{}", self.columns.iter().map(|c| tsv_escape(&c.name)).collect::<Vec<_>>().join("\t"))?;
                for row in &self.rows {
                    let cells: Vec<String> =
                        row.iter().map(|v| tsv_escape(&cut(text(v).unwrap_or_else(|| "null".to_string()), max_cell))).collect();
                    writeln!(out, "{}", cells.join("\t"))?;
                }
            }
            Format::Csv => {
                writeln!(out, "{}", self.columns.iter().map(|c| csv_escape(&c.name)).collect::<Vec<_>>().join(","))?;
                for row in &self.rows {
                    let cells: Vec<String> = row.iter().map(|v| csv_escape(&cut(text(v).unwrap_or_default(), max_cell))).collect();
                    writeln!(out, "{}", cells.join(","))?;
                }
            }
            Format::Json => {
                for row in &self.rows {
                    let fields: Vec<String> = self
                        .columns
                        .iter()
                        .zip(row)
                        .map(|(column, value)| format!("{}:{}", Value::String(column.name.clone()), json_cell(value, &column.ty, max_cell)))
                        .collect();
                    writeln!(out, "{{{}}}", fields.join(","))?;
                }
            }
        }
        Ok(())
    }
}

/// A whole float below 2^53 is an integer that the server sent as a float.
fn number_text(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => format!("{f:.0}"),
        _ => n.to_string(),
    }
}

/// `None` for NULL.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(number_text(n)),
        Value::String(s) => Some(s.clone()),
        other => Some(normalized(other).to_string()),
    }
}

fn normalized(value: &Value) -> Value {
    match value {
        Value::Number(n) => serde_json::from_str(&number_text(n)).unwrap_or_else(|_| value.clone()),
        Value::Array(items) => Value::Array(items.iter().map(normalized).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), normalized(v))).collect()),
        other => other.clone(),
    }
}

fn is_plain_number(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
    !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()) && !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// A `numeric` column arrives as exact text and leaves as a JSON number with all its digits.
fn json_cell(value: &Value, ty: &str, max_cell: usize) -> String {
    match value {
        Value::String(s) if ty == "numeric" && is_plain_number(s) => s.clone(),
        Value::String(s) => Value::String(cut(s.clone(), max_cell)).to_string(),
        other => normalized(other).to_string(),
    }
}

/// A transaction hash (66 characters) or anything shorter is never cut: it is of no use in part.
const NEVER_CUT: usize = 66;

fn cut(text: String, max_cell: usize) -> String {
    let length = text.chars().count();
    if max_cell == 0 || length <= max_cell.max(NEVER_CUT) {
        return text;
    }
    let kept: String = text.chars().take(max_cell).collect();
    format!("{kept}…[+{} chars]", length - max_cell)
}

fn tsv_escape(text: &str) -> String {
    if !text.contains(['\t', '\n', '\r', '\\']) {
        return text.to_string();
    }
    text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn csv_escape(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn table() -> Table {
        let columns = vec![
            Column { name: "tx".into(), ty: "bytea".into() },
            Column { name: "gas".into(), ty: "numeric".into() },
            Column { name: "used".into(), ty: "".into() },
            Column { name: "error".into(), ty: "transaction_error".into() },
            Column { name: "stack".into(), ty: "int4[]".into() },
            Column { name: "note".into(), ty: "text".into() },
        ];
        let rows = vec![
            json!({"tx": "0xab", "gas": "115792089237316195423570985008687907853269984665640564039457584007913129639935",
                   "used": 230000.0, "error": null, "stack": [2, 1.0, 0], "note": "a\tb\n\"c\", d"}),
            json!({"tx": "0xcd", "gas": "0.5", "used": 9007199254740993u64, "error": "ErrOutOfGas", "stack": [], "note": ""}),
        ];
        Table::from_objects(columns, rows)
    }

    fn render(format: Format, max_cell: usize) -> String {
        let mut out = Vec::new();
        table().write(format, max_cell, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn tsv_keeps_every_digit_and_writes_whole_floats_as_integers() {
        assert_eq!(
            render(Format::Tsv, 0),
            "tx\tgas\tused\terror\tstack\tnote\n\
             0xab\t115792089237316195423570985008687907853269984665640564039457584007913129639935\t230000\tnull\t[2,1,0]\ta\\tb\\n\"c\", d\n\
             0xcd\t0.5\t9007199254740993\tErrOutOfGas\t[]\t\n"
        );
    }

    #[test]
    fn json_lines_carry_numeric_columns_as_numbers() {
        assert_eq!(
            render(Format::Json, 0),
            "{\"tx\":\"0xab\",\"gas\":115792089237316195423570985008687907853269984665640564039457584007913129639935,\"used\":230000,\"error\":null,\"stack\":[2,1,0],\"note\":\"a\\tb\\n\\\"c\\\", d\"}\n\
             {\"tx\":\"0xcd\",\"gas\":0.5,\"used\":9007199254740993,\"error\":\"ErrOutOfGas\",\"stack\":[],\"note\":\"\"}\n"
        );
    }

    #[test]
    fn csv_quotes_what_needs_it_and_leaves_null_empty() {
        let csv = render(Format::Csv, 0);
        assert!(csv.starts_with("tx,gas,used,error,stack,note\n0xab,1157"));
        assert!(csv.contains(",230000,,\"[2,1,0]\",\"a\tb\n\"\"c\"\", d\"\n"));
    }

    #[test]
    fn a_cut_cell_says_how_much_is_missing() {
        assert!(render(Format::Tsv, 10).contains("0xab\t1157920892…[+68 chars]\t230000"));
        let hash = format!("0x{}", "ab".repeat(32));
        assert_eq!(cut(hash.clone(), 10), hash);
    }

    #[test]
    fn column_order_falls_back_to_the_first_row() {
        let t = Table::from_objects(Vec::new(), vec![json!({"b": 1, "a": 2}), json!({"a": 3, "b": 4})]);
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(t.rows[1], vec![json!(4), json!(3)]);
    }
}
