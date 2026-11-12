//! Which blocks each history table holds, from which block its rows are stored uncompressed,
//! what is indexed in either part, and which columns the compressed part can skip rows by.
//!
//! The platform is getting an API that reports this. Until it is deployed, the answer is the
//! table in `coverage.tsv`, read from the database on the day its first line names. Rows keep
//! arriving after that day and older blocks get compressed, so a boundary in the table can only
//! be older than the real one.

use serde_json::{Value, json};

use crate::output::Table;

const DATA: &str = include_str!("coverage.tsv");

/// Blocks newer than this many days are never compressed.
pub const ALWAYS_UNCOMPRESSED_DAYS: u64 = 15;

pub const COLUMNS: [&str; 10] = [
    "table",
    "first_block",
    "first_time",
    "last_block",
    "last_time",
    "uncompressed_from_block",
    "uncompressed_from_time",
    "indexed",
    "indexed_compressed",
    "filtered_compressed",
];

/// The tables every chain with rows has, in the order they are printed.
#[cfg(test)]
const HISTORY: [&str; 4] = ["transaction_detail", "logs", "token_ledger", "outer_transaction"];

/// Transactions by the selectors and addresses they involve: not every chain has it.
pub const SIDE_TABLE: &str = "address_selector";

/// One table of one chain, as the columns of `COLUMNS`.
struct Row<'a>(Vec<&'a str>);

impl<'a> Row<'a> {
    fn get(&self, column: &str) -> &'a str {
        COLUMNS.iter().position(|c| *c == column).and_then(|i| self.0.get(i).copied()).unwrap_or("")
    }
}

/// The day the table was read, as `2026-11-12`.
pub fn read_on() -> &'static str {
    DATA.lines().next().and_then(|line| line.strip_prefix("# read on ")).unwrap_or("?").trim()
}

fn rows(chain: &str) -> Vec<Row<'static>> {
    DATA.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, _)| *name == chain)
        .map(|(_, rest)| Row(rest.split('\t').collect()))
        .collect()
}

/// Whether the API refused to read the chain's side table on the day the table was read.
fn side_table_refused(chain: &str) -> bool {
    DATA.lines()
        .filter_map(|line| line.strip_prefix("# address_selector refused: "))
        .any(|chains| chains.split(' ').any(|name| name == chain))
}

/// Seconds since 1970 of a time written as `2026-07-24T03:46:47Z`.
fn epoch(time: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| time.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13).unwrap_or(0), number(14..16).unwrap_or(0), number(17..19).unwrap_or(0));
    // The day count of a civil date, after Howard Hinnant's `days_from_civil`.
    let y = if month <= 2 { year - 1 } else { year };
    let (era, year_of_era) = (y.div_euclid(400), y.rem_euclid(400));
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some((era * 146_097 + day_of_era - 719_468) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Seconds of a window such as `24h`, `30d` or `2 day`.
pub fn window_seconds(window: &str) -> Option<i64> {
    let window = window.trim();
    let digits = window.bytes().take_while(u8::is_ascii_digit).count();
    let count: i64 = window[..digits].parse().ok()?;
    let unit = match window[digits..].trim().trim_end_matches('s') {
        "" | "sec" | "second" => 1,
        "m" | "min" | "minute" => 60,
        "h" | "hr" | "hour" => 3600,
        "d" | "day" => 86_400,
        "w" | "week" => 7 * 86_400,
        "mon" | "month" => 30 * 86_400,
        "y" | "yr" | "year" => 365 * 86_400,
        _ => return None,
    };
    Some(count * unit)
}

/// Where the uncompressed rows of a table began when it was read.
pub struct Boundary {
    pub block: &'static str,
    /// The day, as `2026-07-24`.
    pub day: &'static str,
}

/// The boundary of `table`, if a window of `last` ending at `now` starts before it: such a
/// window surely reads compressed rows, as the boundary only moves forward.
pub fn compressed_reach(chain: &str, table: &str, last: &str, now: i64) -> Option<Boundary> {
    let row = rows(chain).into_iter().find(|row| row.get("table") == table)?;
    if row.get("indexed_compressed").is_empty() {
        return None;
    }
    let time = row.get("uncompressed_from_time");
    let starts = now - window_seconds(last)?;
    (starts < epoch(time)?).then(|| Boundary { block: row.get("uncompressed_from_block"), day: &time[..10] })
}

fn cell(text: &str) -> Value {
    match text.parse::<u64>() {
        Ok(number) => json!(number),
        Err(_) if text.is_empty() => Value::Null,
        Err(_) => json!(text),
    }
}

/// The table of one chain, and what to say about it.
pub fn table(chain: &str) -> (Table, Vec<String>) {
    let mut table = Table::new(&COLUMNS);
    let mut notes = Vec::new();
    let read = epoch(read_on()).unwrap_or(0);
    for row in rows(chain) {
        table.rows.push(row.0.iter().map(|text| cell(text)).collect());
        let last = row.get("last_time");
        if epoch(last).is_some_and(|at| read - at > 2 * 86_400) {
            notes.push(format!("`{}` was no longer written when read: its last row is of {}", row.get("table"), &last[..10]));
        }
    }
    if table.rows.is_empty() {
        notes.push(format!("the history tables of {chain} hold no rows"));
    } else if !rows(chain).iter().any(|row| row.get("table") == SIDE_TABLE) {
        notes.push(format!("{chain} has no `{SIDE_TABLE}`"));
    } else if side_table_refused(chain) {
        notes.push(format!("`{SIDE_TABLE}` holds these blocks, but the API refused to read it when this was read (permission denied)"));
    }
    (table, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_read_as_seconds() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-07-24T03:46:47Z"), Some(1_784_864_807));
        assert_eq!(epoch("2026-11-12"), Some(1_794_441_600));
        assert_eq!(epoch(""), None);
    }

    #[test]
    fn windows_are_read_as_seconds() {
        assert_eq!(window_seconds("24h"), Some(86_400));
        assert_eq!(window_seconds("365d"), Some(365 * 86_400));
        assert_eq!(window_seconds("2 day"), Some(2 * 86_400));
        assert_eq!(window_seconds("3 weeks"), Some(21 * 86_400));
        assert_eq!(window_seconds("soon"), None);
    }

    #[test]
    fn every_chain_with_rows_has_the_four_tables_and_sane_ranges() {
        assert!(epoch(read_on()).is_some());
        for chain in ["ethereum", "optimism", "binance", "polygon", "robinhood", "arc", "base", "arbitrum", "avalanche", "blast"] {
            let found = rows(chain);
            let names: Vec<&str> = found.iter().map(|row| row.get("table")).collect();
            assert_eq!(names[..4], HISTORY, "{chain}");
            assert_eq!(names[4..], if chain == "blast" { vec![] } else { vec![SIDE_TABLE] }, "{chain}");
            for row in &found {
                assert_eq!(row.0.len(), COLUMNS.len(), "{chain} {}", row.get("table"));
                let block = |column: &str| row.get(column).parse::<u64>().unwrap();
                assert!(block("first_block") <= block("uncompressed_from_block"));
                assert!(block("uncompressed_from_block") <= block("last_block"));
                assert!(row.get("indexed").contains("block_number"));
                // A filter only exists where rows are compressed, and never on the column they are stored by.
                let (order, filtered) = (row.get("indexed_compressed"), row.get("filtered_compressed"));
                assert!(filtered.is_empty() || !order.is_empty(), "{chain} {}", row.get("table"));
                let lead = order.trim_start_matches('(').split(',').next().unwrap_or("");
                assert!(filtered.is_empty() || filtered.split(", ").all(|column| column != lead), "{chain} {}", row.get("table"));
            }
        }
        assert!(rows("fantom").is_empty());
        assert_eq!(table("fantom").1, ["the history tables of fantom hold no rows"]);
    }

    #[test]
    fn filters_and_the_side_table_are_as_read() {
        let of = |chain: &str, table: &str, column: &str| {
            rows(chain).into_iter().find(|row| row.get("table") == table).map(|row| row.get(column)).unwrap_or("absent")
        };
        // Every chain can skip compressed frames by their caller; only two can skip events by topic.
        for chain in ["ethereum", "base", "arbitrum", "arc"] {
            assert_eq!(of(chain, "transaction_detail", "filtered_compressed"), "from_a", "{chain}");
        }
        assert_eq!(of("ethereum", "logs", "filtered_compressed"), "topic0, vm_step");
        assert_eq!(of("arbitrum", "logs", "filtered_compressed"), "topic0, vm_step");
        assert_eq!(of("base", "logs", "filtered_compressed"), "");
        assert_eq!(of("ethereum", "token_ledger", "filtered_compressed"), "");
        assert_eq!(of("ethereum", SIDE_TABLE, "indexed_compressed"), "(selector, block_number, tx_index)");
        // Base holds a few stray rows seven million blocks before the table is filled.
        assert_eq!(of("base", SIDE_TABLE, "first_block"), "44091527");
        assert_eq!(of("blast", SIDE_TABLE, "first_block"), "absent");
        assert_eq!(table("blast").1.last().unwrap(), "blast has no `address_selector`");
        assert!(table("arbitrum").1.last().unwrap().contains("the API refused to read it"));
        assert!(table("base").1.is_empty());
    }

    #[test]
    fn a_window_that_starts_before_the_boundary_reads_compressed_rows() {
        let now = epoch("2026-11-12T12:00:00Z").unwrap();
        let reach = compressed_reach("ethereum", "transaction_detail", "365d", now).unwrap();
        assert_eq!((reach.block, reach.day), ("25600000", "2026-07-24"));
        assert!(compressed_reach("ethereum", "transaction_detail", "100d", now).is_none());
        // Never compressed on this chain.
        assert!(compressed_reach("ethereum", "outer_transaction", "3650d", now).is_none());
        assert!(compressed_reach("arc", "outer_transaction", "365d", now).is_some());
    }

    #[test]
    fn a_table_that_stopped_is_said_to_have_stopped() {
        let (table, notes) = table("blast");
        assert_eq!(table.rows.len(), 4);
        assert_eq!(notes.len(), 5);
        assert!(notes[0].contains("`transaction_detail` was no longer written when read: its last row is of 2026-10-29"));
        assert!(super::table("ethereum").1.is_empty());
    }
}
