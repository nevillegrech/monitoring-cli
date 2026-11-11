//! Call frames of `transaction_detail`: calls to a contract and calls made by one.
//!
//! One index scan, newest first, picks the rows. A filter on a column outside that index
//! (failed, top-level, the other party) runs over the newest `--scan` rows of the window, so
//! that a rare match on a busy contract cannot read all of a long window. Columns of other
//! rows (the transaction, the calling frame) are read by primary key for the rows that are
//! kept: at most 500 lookups, whatever the planner would make of a join.

use std::collections::BTreeMap;

use alloy_json_abi::Function;
use alloy_primitives::hex;
use serde_json::{Value, json};

use crate::api::Session;
use crate::error::{Fail, Res};
use crate::lookups::{self, ContractAbi, any_of, bytes_cell, cell, frame_status};
use crate::output::Table;
use crate::run::{self, QueryOpts};
use crate::sig;

/// Which side of the call the index leads with.
#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    /// Calls to a contract: the `(to_a, selector, block_number)` index.
    To,
    /// Calls made by a contract: the `(from_a, block_number)` index.
    From,
}

pub struct CallsArgs {
    pub side: Side,
    /// The contract on the leading side.
    pub address: String,
    /// The address on the other side, if it is restricted.
    pub other: Option<String>,
    pub function: Option<String>,
    pub failed: bool,
    /// Failed calls whose calling frame went on without an error.
    pub tolerated: bool,
    pub top_level: bool,
    /// Add what each call returned: its outputs, or the revert reason of a failed call.
    pub returns: bool,
    /// Add the sender, gas limit, gas used and status of each call's transaction.
    pub with_tx: bool,
    /// How many of the newest calls the unindexed filters look at.
    pub scan: u32,
    pub raw: bool,
}

fn selectors_of(function: &str, abi: &ContractAbi) -> Res<Vec<[u8; 4]>> {
    if let Some(selector) = lookups::hex_of(function, 8) {
        let bytes = sig::unhex(&selector).expect("checked hex");
        return Ok(vec![[bytes[0], bytes[1], bytes[2], bytes[3]]]);
    }
    if function.contains('(') {
        return Ok(vec![sig::parse_function(function)?.selector().0]);
    }
    let named: Vec<[u8; 4]> = abi.functions.iter().filter(|(_, f)| f.name == function).map(|(s, _)| *s).collect();
    if named.is_empty() {
        let mut names: Vec<&str> = abi.functions.values().map(|f| f.name.as_str()).collect();
        names.sort();
        names.dedup();
        let known =
            if names.is_empty() { "the contract has no known ABI".to_string() } else { format!("its ABI has: {}", names.join(", ")) };
        return Err(Fail::Usage(format!(
            "no function named `{function}` in the contract's ABI ({known}). Give a signature such as `transfer(address,uint256)` or a 0x selector."
        )));
    }
    Ok(named)
}

/// The most failed calls whose calling frame is looked up when only tolerated ones are wanted.
/// The lookups stop as soon as enough rows pass.
const TOLERATED_CANDIDATES: u32 = 500;

const FRAME_COLUMNS: &str = "d.block_number, d.tx_index, d.vm_step_start, d.from_a, d.to_a, d.caller_vm_step_stack, d.call_opcode,\n           \
                             d.gas, d.gas_used, d.callvalue, d.error, d.committed, d.calldata";

struct FrameQuery {
    last: String,
    /// Conditions the leading index answers.
    indexed: Vec<String>,
    /// Conditions on other columns, written over the alias `s`.
    filters: Vec<String>,
    returns: bool,
    with_tx: bool,
    with_parent: bool,
    tolerated: bool,
    scan: u32,
    rows: u32,
}

impl FrameQuery {
    fn source(&self) -> String {
        format!("{{{{transaction_detail(duration='{}', only_committed=False)}}}} d", self.last)
    }

    /// The calling frame of a row of `r`, as one JSON column. It is a correlated scalar
    /// subquery on the primary key, through the macro: the macro's block bound keeps each
    /// lookup inside the window's chunks, and a subquery runs once per row. Written as a join
    /// (LATERAL included), the planner hashes every frame of the window.
    fn parent(&self) -> String {
        format!(
            ",\n               (SELECT jsonb_build_object('selector', encode(substring(q.calldata FROM 1 FOR 4), 'hex'), 'error', q.error, 'committed', q.committed)\n                \
             FROM {{{{transaction_detail(duration='{}', only_committed=False)}}}} q\n                \
             WHERE q.block_number = r.block_number AND q.tx_index = r.tx_index\n                  \
             AND q.vm_step_start = r.caller_vm_step_stack[1]) AS parent",
            self.last
        )
    }

    fn sql(&self) -> String {
        let columns = if self.returns { format!("{FRAME_COLUMNS}, d.returndata") } else { FRAME_COLUMNS.to_string() };
        let kept = if self.tolerated { TOLERATED_CANDIDATES.max(self.rows) } else { self.rows };
        let scan = |limit: u32| {
            format!(
                "SELECT {columns}\n    FROM {}\n    WHERE {}\n    ORDER BY d.block_number DESC\n    LIMIT {limit}",
                self.source(),
                self.indexed.join("\n      AND ")
            )
        };
        let rows = if self.filters.is_empty() {
            scan(kept)
        } else {
            format!(
                "SELECT s.*\n    FROM (\n        {}\n    ) s\n    WHERE {}\n    ORDER BY s.block_number DESC\n    LIMIT {kept}",
                scan(self.scan).replace('\n', "\n    "),
                self.filters.join("\n      AND ")
            )
        };
        let mut select = "SELECT tx_hash(x.block_number, x.tx_index) AS tx, x.block_number AS block, ts(x.block_number) AS time,\n       \
                          x.from_a AS caller, x.to_a AS callee, cardinality(x.caller_vm_step_stack) AS depth, x.call_opcode AS op,\n       \
                          x.gas, x.gas_used, x.callvalue AS value, x.error, x.committed, x.calldata"
            .to_string();
        if self.returns {
            select.push_str(", x.returndata");
        }
        if self.with_parent {
            select.push_str(", x.parent");
        }
        if self.with_tx {
            select.push_str(&lookups::tx_info("x"));
        }
        // No ORDER BY out here: the rows arrive newest first, and the limit must be able to
        // stop the per-row lookups below it.
        let (parent, fence) = if self.with_parent { (self.parent(), "\n    OFFSET 0") } else { (String::new(), "") };
        let tolerated = if self.tolerated { "\nWHERE x.parent->>'error' IS NULL" } else { "" };
        format!(
            "{select}\nFROM (\n    SELECT r.*{parent}\n    FROM (\n        {}\n    ) r{fence}\n) x{tolerated}\nLIMIT {}",
            rows.replace('\n', "\n    "),
            self.rows
        )
    }

    /// The oldest call of the sample, if the sample is full.
    fn sample_end(&self) -> String {
        format!(
            "SELECT d.block_number AS block, ts(d.block_number) AS time\nFROM {}\nWHERE {}\nORDER BY d.block_number DESC\nOFFSET {}\nLIMIT 1",
            self.source(),
            self.indexed.join("\n  AND "),
            self.scan - 1
        )
    }
}

/// The function's name and its arguments. A call without calldata, a plain transfer of ETH,
/// has neither.
fn call_text(function: Option<&Function>, calldata: &[u8]) -> (String, String) {
    if calldata.is_empty() {
        return (String::new(), String::new());
    }
    let raw_args = || format!("0x{}", hex::encode(calldata.get(4..).unwrap_or_default()));
    match function {
        Some(function) => (function.name.clone(), sig::decode_call(function, calldata).unwrap_or_else(raw_args)),
        None => (format!("0x{}", hex::encode(calldata.get(..4).unwrap_or(calldata))), raw_args()),
    }
}

pub fn calls(session: &mut Session, args: &CallsArgs, opts: &QueryOpts) -> Res<Table> {
    let lead = lookups::address(&args.address)?;
    let abi = lookups::contract_abi(session, &opts.chain, &lead)?;
    if let Some(implementation) = &abi.implementation {
        run::note(&format!("{lead} is a proxy; its functions are named with the ABI of its implementation {implementation}"));
    }
    let mut indexed = Vec::new();
    let mut filters = Vec::new();
    let other = args.other.as_deref().map(lookups::address).transpose()?;
    match args.side {
        Side::To => {
            indexed.push(format!("d.to_a = {lead}"));
            if let Some(function) = &args.function {
                let selectors: Vec<String> = selectors_of(function, &abi)?.iter().map(|s| sig::bytea(s)).collect();
                indexed.push(any_of("selector(d.calldata)", &selectors));
            }
            filters.extend(other.map(|caller| format!("s.from_a = {caller}")));
        }
        Side::From => {
            indexed.push(format!("d.from_a = {lead}"));
            filters.extend(other.map(|callee| format!("s.to_a = {callee}")));
        }
    }
    if args.failed || args.tolerated {
        filters.push("s.error IS NOT NULL".to_string());
    }
    if args.top_level {
        filters.push("s.vm_step_start = 0".to_string());
    }
    let rows = opts.limit + opts.offset;
    let query = FrameQuery {
        last: opts.last.clone(),
        indexed,
        filters,
        returns: args.returns,
        with_tx: args.with_tx,
        with_parent: args.side == Side::From,
        tolerated: args.tolerated,
        scan: args.scan.max(rows),
        rows,
    };
    let fetched = run::query(session, &query.sql(), opts)?.table;
    if !query.filters.is_empty() && (fetched.rows.len() as u32) < opts.limit {
        // Fewer rows than asked for: say whether the whole window was searched.
        let probe = QueryOpts { limit: 1, offset: 0, show_sql: false, ..opts.clone() };
        if let Some(end) = run::query(session, &query.sample_end(), &probe).ok().and_then(|o| o.table.rows.into_iter().next()) {
            run::note(&format!(
                "the filters looked at the newest {} calls only, back to block {} ({}); --scan N looks further back",
                query.scan,
                end[0],
                end[1].as_str().unwrap_or("?")
            ));
        }
    }
    if args.raw {
        return Ok(fetched);
    }
    let selector_in = |row: &[Value], column: &str| -> Option<[u8; 4]> { bytes_cell(&fetched, row, column).get(..4)?.try_into().ok() };
    // The lead contract's ABI names the functions called on it; calls it makes go to others.
    let own = |selector: &[u8; 4]| if args.side == Side::To { abi.functions.get(selector) } else { None };
    let mut unknown: Vec<[u8; 4]> = fetched.rows.iter().filter_map(|r| selector_in(r, "calldata")).filter(|s| own(s).is_none()).collect();
    unknown.sort();
    unknown.dedup();
    let searched = lookups::functions_by_selector(session, &unknown);
    let counterparty = if args.side == Side::To { "caller" } else { "callee" };
    let mut names = vec!["tx", "block", "time", counterparty, "depth", "op", "gas", "gas_used", "value", "status", "function", "args"];
    if args.returns {
        names.push("returned");
    }
    if args.side == Side::From {
        names.extend(["caller_function", "caller_status"]);
    }
    if args.with_tx {
        names.extend(lookups::TX_NAMES);
    }
    let mut table = Table::new(&names);
    for column in table.columns.iter_mut() {
        if ["gas", "gas_used", "value", "tx_gas_limit", "tx_gas_used"].contains(&column.name.as_str()) {
            column.ty = "numeric".to_string();
        }
    }
    let mut errors = BTreeMap::new();
    for row in &fetched.rows {
        let calldata = bytes_cell(&fetched, row, "calldata");
        let function = selector_in(row, "calldata").and_then(|s| own(&s).or_else(|| searched.get(&s)));
        let (name, decoded) = call_text(function, &calldata);
        let mut out: Vec<Value> = ["tx", "block", "time", counterparty, "depth", "op", "gas", "gas_used", "value"]
            .iter()
            .map(|c| cell(&fetched, row, c).clone())
            .collect();
        out.push(frame_status(cell(&fetched, row, "error"), cell(&fetched, row, "committed")));
        out.push(json!(name));
        out.push(json!(decoded));
        if args.returns {
            let returndata = bytes_cell(&fetched, row, "returndata");
            out.push(json!(if cell(&fetched, row, "error").is_null() {
                function
                    .and_then(|f| sig::decode_return(f, &returndata))
                    .unwrap_or_else(|| if returndata.is_empty() { String::new() } else { format!("0x{}", hex::encode(&returndata)) })
            } else {
                lookups::revert_reason(session, &mut errors, &returndata)
            }));
        }
        if args.side == Side::From {
            // The calling frame runs the lead contract's code, so its ABI names the function.
            let parent = cell(&fetched, row, "parent");
            let selector: Option<[u8; 4]> = parent["selector"].as_str().and_then(sig::unhex).and_then(|b| b.try_into().ok());
            if parent.is_null() {
                out.extend([Value::Null, Value::Null]);
            } else {
                out.push(json!(match selector {
                    Some(selector) =>
                        abi.functions.get(&selector).map(|f| f.name.clone()).unwrap_or_else(|| format!("0x{}", hex::encode(selector))),
                    None => String::new(),
                }));
                out.push(frame_status(&parent["error"], &parent["committed"]));
            }
        }
        if args.with_tx {
            out.extend(lookups::tx_cells(&fetched, row));
        }
        table.rows.push(out);
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> FrameQuery {
        FrameQuery {
            last: "30d".to_string(),
            indexed: vec!["d.to_a = 0x00000000000000000000000000000000000000aa".to_string()],
            filters: Vec::new(),
            returns: false,
            with_tx: false,
            with_parent: false,
            tolerated: false,
            scan: 20000,
            rows: 20,
        }
    }

    #[test]
    fn a_plain_lookup_is_one_index_scan_with_a_limit() {
        let sql = query().sql();
        assert!(sql.contains("FROM {{transaction_detail(duration='30d', only_committed=False)}} d\n        WHERE d.to_a = 0x00000000000000000000000000000000000000aa\n        ORDER BY d.block_number DESC\n        LIMIT 20\n    ) r\n) x\nLIMIT 20"));
        assert!(!sql.contains("JOIN"));
        assert!(!sql.contains(") s"));
        assert!(!sql.contains("jsonb_build_object"));
    }

    #[test]
    fn unindexed_filters_run_over_a_bounded_sample() {
        let q = FrameQuery { filters: vec!["s.error IS NOT NULL".to_string()], ..query() };
        let sql = q.sql();
        assert!(sql.contains("LIMIT 20000\n        ) s\n        WHERE s.error IS NOT NULL\n        ORDER BY s.block_number DESC\n        LIMIT 20\n    ) r\n) x"));
        assert!(q.sample_end().ends_with("ORDER BY d.block_number DESC\nOFFSET 19999\nLIMIT 1"));
    }

    #[test]
    fn other_tables_are_read_by_scalar_subqueries_on_the_primary_key() {
        let sql =
            FrameQuery { with_tx: true, with_parent: true, tolerated: true, filters: vec!["s.error IS NOT NULL".to_string()], ..query() }
                .sql();
        assert!(
            sql.contains("FROM outer_transaction o\n        WHERE o.block_number = x.block_number AND o.tx_index = x.tx_index) AS tx_info")
        );
        assert!(sql.contains("FROM {{transaction_detail(duration='30d', only_committed=False)}} q\n                WHERE q.block_number = r.block_number AND q.tx_index = r.tx_index\n                  AND q.vm_step_start = r.caller_vm_step_stack[1]) AS parent"));
        assert!(sql.contains("LIMIT 500\n    ) r\n    OFFSET 0\n) x\nWHERE x.parent->>'error' IS NULL\nLIMIT 20"));
        assert!(!sql.contains("JOIN"));
        assert!(!sql.contains("ORDER BY x."));
    }

    #[test]
    fn a_function_is_found_by_selector_signature_or_name() {
        let transfer = sig::parse_function("transfer(address to, uint256 value)").unwrap();
        let abi = ContractAbi { functions: BTreeMap::from([(transfer.selector().0, transfer)]), implementation: None };
        let expected = vec![[0xa9, 0x05, 0x9c, 0xbb]];
        assert_eq!(selectors_of("0xA9059CBB", &abi).unwrap(), expected);
        assert_eq!(selectors_of("transfer(address,uint256)", &abi).unwrap(), expected);
        assert_eq!(selectors_of("transfer", &abi).unwrap(), expected);
        assert!(matches!(selectors_of("mint", &abi), Err(Fail::Usage(m)) if m.contains("its ABI has: transfer")));
    }

    #[test]
    fn a_call_shows_its_function_and_arguments_or_the_raw_bytes() {
        let transfer = sig::parse_function("transfer(address to, uint256 value)").unwrap();
        let calldata = hex::decode(format!("a9059cbb{:0>64}{:0>64}", "bb", "64")).unwrap();
        assert_eq!(
            call_text(Some(&transfer), &calldata),
            ("transfer".to_string(), "to=0x00000000000000000000000000000000000000bb, value=100".to_string())
        );
        assert_eq!(call_text(None, &calldata[..6]), ("0xa9059cbb".to_string(), "0x0000".to_string()));
        assert_eq!(call_text(None, &[]), (String::new(), String::new()));
    }
}
