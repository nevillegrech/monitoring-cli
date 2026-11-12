//! Call frames of `transaction_detail`: calls to a contract and calls made by one.
//!
//! One index scan, newest first, picks the rows. A filter on a column outside that index
//! (failed, top-level, the caller) runs over the newest `--scan` rows of the window, so
//! that a rare match on a busy contract cannot read all of a long window. Columns of other
//! rows (the transaction, the calling frame) are read by primary key for the rows that are
//! kept: at most 500 lookups, whatever the planner would make of a join.

use std::collections::BTreeMap;

use alloy_json_abi::Function;
use alloy_primitives::hex;
use serde_json::{Value, json};

use crate::api::{self, Session};
use crate::coverage;
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
    /// Calls made by a contract: the `(from_a, block_number)` index. Compressed blocks have
    /// none on the caller: they are stored by callee, and read by it when one is given.
    From,
}

/// What `--count-by` groups the calls by.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum By {
    Function,
    Caller,
    Callee,
    Status,
}

impl By {
    fn name(self) -> &'static str {
        match self {
            By::Function => "function",
            By::Caller => "caller",
            By::Callee => "callee",
            By::Status => "status",
        }
    }

    /// The key over a row of the sample `s`. The status reads as it does in the listing.
    fn expr(self) -> &'static str {
        match self {
            // A creation's calldata is init code, which starts with no selector.
            By::Function => "CASE WHEN s.call_opcode IN ('CREATE', 'CREATE2') THEN NULL ELSE s.sel END",
            By::Caller => "s.from_a",
            By::Callee => "s.to_a",
            // `error` is an enum: as text it can share a column with the two other words.
            By::Status => "CASE WHEN s.error IS NOT NULL THEN s.error::text WHEN s.committed THEN 'ok' ELSE 'reverted' END",
        }
    }
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
    /// Count the calls per these keys instead of listing them.
    pub count_by: Vec<By>,
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

/// What a count needs of a call: the selector, not the calldata.
const COUNT_COLUMNS: &str = "d.block_number, d.vm_step_start, d.from_a, d.to_a, d.call_opcode, d.gas, d.gas_used, d.error, d.committed,\n           \
                             selector(d.calldata) AS sel";

#[derive(Clone)]
struct FrameQuery {
    last: String,
    /// Conditions the leading index answers.
    indexed: Vec<String>,
    /// The newest block to read, for the pages after the first.
    until: Option<u64>,
    /// Conditions on other columns, written over the alias `s`.
    filters: Vec<String>,
    returns: bool,
    with_tx: bool,
    with_parent: bool,
    /// The contracts whose code most likely made the calls: the calling frame is looked for
    /// under them first.
    parent_in: Vec<String>,
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
    ///
    /// Compressed blocks have no index on the key: a frame is found there through the contract
    /// it ran in, and by its position alone only with a scan of its blocks. So the frame is
    /// looked for under `parent_in` first, and by position alone only if it is not there (the
    /// call was made by a library or by an earlier implementation).
    fn parent(&self) -> String {
        let frame = |within: &str| {
            format!(
                "(SELECT jsonb_build_object('selector', encode(substring(q.calldata FROM 1 FOR 4), 'hex'), 'error', q.error, 'committed', q.committed)\n                \
                 FROM {{{{transaction_detail(duration='{}', only_committed=False)}}}} q\n                \
                 WHERE {within}q.block_number = r.block_number AND q.tx_index = r.tx_index\n                  \
                 AND q.vm_step_start = r.caller_vm_step_stack[1])",
                self.last
            )
        };
        if self.parent_in.is_empty() {
            return format!(",\n               {} AS parent", frame(""));
        }
        let within = format!("q.to_a = ANY (ARRAY[{}]) AND ", self.parent_in.join(", "));
        format!(",\n               COALESCE(\n               {},\n               {}) AS parent", frame(&within), frame(""))
    }

    /// The newest `limit` calls that the index leads to.
    fn newest(&self, columns: &str, limit: u32) -> String {
        let until = self.until.map(|block| format!("\n      AND d.block_number <= {block}")).unwrap_or_default();
        format!(
            "SELECT {columns}\n    FROM {}\n    WHERE {}{until}\n    ORDER BY d.block_number DESC\n    LIMIT {limit}",
            self.source(),
            self.indexed.join("\n      AND ")
        )
    }

    /// Calls counted per key. An aggregate reads every row it is given, so like the filters it
    /// gets the newest `scan` calls of the window, not the window.
    fn count_sql(&self, keys: &[By]) -> String {
        let named: Vec<String> = keys.iter().enumerate().map(|(i, key)| format!("{} AS k{}", key.expr(), i + 1)).collect();
        let positions: Vec<String> = (1..=keys.len()).map(|i| i.to_string()).collect();
        let outer: Vec<String> = (1..=keys.len()).map(|i| format!("g.k{i}")).collect();
        let filters = if self.filters.is_empty() { String::new() } else { format!("\n    WHERE {}", self.filters.join("\n      AND ")) };
        format!(
            "SELECT g.*, ts(g.first_block) AS first, ts(g.last_block) AS last\n\
             FROM (\n    \
             SELECT {},\n           \
             count(*) AS calls, count(*) FILTER (WHERE s.error IS NOT NULL) AS failed,\n           \
             min(s.gas) AS gas_min, max(s.gas) AS gas_max, max(s.gas_used) AS gas_used_max,\n           \
             min(s.block_number) AS first_block, max(s.block_number) AS last_block\n    \
             FROM (\n        {}\n    ) s{filters}\n    \
             GROUP BY {}\n    \
             ORDER BY calls DESC, {}\n    \
             LIMIT {}\n\
             ) g\n\
             ORDER BY g.calls DESC, {}",
            named.join(", "),
            self.newest(COUNT_COLUMNS, self.scan).replace('\n', "\n    "),
            positions.join(", "),
            positions.join(", "),
            self.rows,
            outer.join(", "),
        )
    }

    fn sql(&self) -> String {
        let columns = if self.returns { format!("{FRAME_COLUMNS}, d.returndata") } else { FRAME_COLUMNS.to_string() };
        let kept = if self.tolerated { TOLERATED_CANDIDATES.max(self.rows) } else { self.rows };
        let scan = |limit: u32| self.newest(&columns, limit);
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

/// The keys of a count, each once. The lead contract is the same on every row.
fn count_keys(side: Side, given: &[By]) -> Res<Vec<By>> {
    let (lead, command, other) = if side == Side::To { (By::Callee, "calls", "caller") } else { (By::Caller, "subcalls", "callee") };
    if given.contains(&lead) {
        return Err(Fail::Usage(format!(
            "the {} of every row is the contract itself: `{command}` counts by function, {other} or status",
            lead.name()
        )));
    }
    let mut keys: Vec<By> = Vec::new();
    for key in given {
        if !keys.contains(key) {
            keys.push(*key);
        }
    }
    Ok(keys)
}

/// The function of a selector, named apart from another selector's function of the same name.
fn function_names(selectors: &[[u8; 4]], known: impl Fn(&[u8; 4]) -> Option<String>) -> BTreeMap<[u8; 4], String> {
    let named: Vec<([u8; 4], Option<String>)> = selectors.iter().map(|s| (*s, known(s))).collect();
    named
        .iter()
        .map(|(selector, name)| {
            let hex = format!("0x{}", hex::encode(selector));
            let text = match name {
                Some(name) if named.iter().filter(|(_, other)| other.as_ref() == Some(name)).count() > 1 => format!("{name} {hex}"),
                Some(name) => name.clone(),
                None => hex,
            };
            (*selector, text)
        })
        .collect()
}

/// The calls of the sample counted per key, with the gas they were given and used.
fn counts(session: &mut Session, side: Side, abi: &ContractAbi, query: &FrameQuery, keys: &[By], opts: &QueryOpts) -> Res<Table> {
    let fetched = run::query(session, &query.count_sql(keys), opts)?.table;
    // When every group is shown and together they hold fewer calls than the sample takes, the
    // sample was the whole window: there is no end to look for.
    let counted: u64 = fetched.rows.iter().filter_map(|row| cell(&fetched, row, "calls").as_u64()).sum();
    let whole = query.filters.is_empty() && (fetched.rows.len() as u32) < opts.limit && counted < u64::from(query.scan);
    let probe = QueryOpts { limit: 1, offset: 0, show_sql: false, ..opts.clone() };
    let end =
        if whole { None } else { run::query(session, &query.sample_end(), &probe).ok().and_then(|o| o.table.rows.into_iter().next()) };
    if let Some(end) = end {
        run::note(&format!(
            "counted over the newest {} calls only, back to block {} ({}); --scan N counts further back",
            query.scan,
            end[0],
            end[1].as_str().unwrap_or("?")
        ));
    }
    let key_column = |i: usize| format!("k{}", i + 1);
    let selector_of = |row: &[Value], i: usize| -> Option<[u8; 4]> { bytes_cell(&fetched, row, &key_column(i)).get(..4)?.try_into().ok() };
    let function_key = keys.iter().position(|key| *key == By::Function);
    let mut selectors: Vec<[u8; 4]> = fetched.rows.iter().filter_map(|row| selector_of(row, function_key?)).collect();
    selectors.sort();
    selectors.dedup();
    // The lead contract's ABI names the functions called on it; calls it makes go to others.
    let own = |selector: &[u8; 4]| if side == Side::To { abi.functions.get(selector) } else { None };
    let unknown: Vec<[u8; 4]> = selectors.iter().filter(|s| own(s).is_none()).copied().collect();
    let searched = lookups::functions_by_selector(session, &unknown);
    let functions = function_names(&selectors, |s| own(s).or_else(|| searched.get(s)).map(|f| f.name.clone()));
    const COUNTS: [&str; 7] = ["calls", "failed", "gas_min", "gas_max", "gas_used_max", "first", "last"];
    let names: Vec<&str> = keys.iter().map(|key| key.name()).chain(COUNTS).collect();
    let mut table = Table::new(&names);
    for column in table.columns.iter_mut().filter(|c| c.name.starts_with("gas")) {
        column.ty = "numeric".to_string();
    }
    for row in &fetched.rows {
        let mut out: Vec<Value> = Vec::with_capacity(names.len());
        for (i, key) in keys.iter().enumerate() {
            let value = cell(&fetched, row, &key_column(i));
            out.push(match key {
                // Two groups have no function: creations, and calls without calldata (a plain
                // transfer of ETH). The listing tells them apart by its `op` column.
                By::Function if value.is_null() => json!("(creation)"),
                By::Function => json!(selector_of(row, i).map(|s| functions[&s].clone()).unwrap_or_else(|| "(no selector)".to_string())),
                _ => value.clone(),
            });
        }
        out.extend(COUNTS.iter().map(|c| cell(&fetched, row, c).clone()));
        table.rows.push(out);
    }
    Ok(table)
}

pub fn calls(session: &mut Session, args: &CallsArgs, opts: &QueryOpts) -> Res<Table> {
    let lead = lookups::address(&args.address)?;
    let count_by = count_keys(args.side, &args.count_by)?;
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
            indexed.extend(other.map(|callee| format!("d.to_a = {callee}")));
        }
    }
    // By caller alone, a window that reaches compressed blocks reads all of them.
    let by_caller = (args.side == Side::From && args.other.is_none())
        .then(|| coverage::compressed_reach(&opts.chain, "transaction_detail", &opts.last, api::now() as i64))
        .flatten()
        .map(|from| {
            format!(
                "calls are found by their caller only in the uncompressed blocks, which began at block {} ({}) when read on {}. \
                 Older blocks are stored by callee: over `--last {}` give --to 0xCALLEE, or shorten --last \
                 (`dedaub-monitoring coverage` shows the ranges, `guide old-data` the ways to ask)",
                from.block,
                from.day,
                coverage::read_on(),
                opts.last
            )
        });
    if let Some(text) = &by_caller {
        run::note(text);
    }
    let hinted = |result: Res<Table>| match (result, &by_caller) {
        (Err(Fail::Timeout(message)), Some(text)) => Err(Fail::Timeout(format!("{message}\nHere, {text}."))),
        (result, _) => result,
    };
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
        until: None,
        filters,
        returns: args.returns,
        with_tx: args.with_tx,
        with_parent: args.side == Side::From,
        // A call is made by the contract's own code, or behind a proxy by its implementation's.
        parent_in: std::iter::once(lead.clone()).chain(abi.implementation.clone()).collect(),
        tolerated: args.tolerated,
        scan: args.scan.max(rows),
        rows,
    };
    if !count_by.is_empty() {
        return hinted(counts(session, args.side, &abi, &query, &count_by, opts));
    }
    let fetched = hinted(run::newest_first(session, opts, |until| FrameQuery { until, ..query.clone() }.sql()))?;
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
    let creation = |row: &[Value]| lookups::is_creation(cell(&fetched, row, "op"));
    let selector_in = |row: &[Value], column: &str| -> Option<[u8; 4]> {
        if creation(row) {
            return None;
        }
        bytes_cell(&fetched, row, column).get(..4)?.try_into().ok()
    };
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
        let (name, decoded) =
            if creation(row) { (String::new(), format!("0x{}", hex::encode(&calldata))) } else { call_text(function, &calldata) };
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
            until: None,
            filters: Vec::new(),
            returns: false,
            with_tx: false,
            with_parent: false,
            parent_in: Vec::new(),
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
    fn pages_after_the_first_stop_at_its_newest_block() {
        let sql = FrameQuery { until: Some(26_000_000), ..query() }.sql();
        assert!(sql.contains(
            "WHERE d.to_a = 0x00000000000000000000000000000000000000aa\n          AND d.block_number <= 26000000\n        \
             ORDER BY d.block_number DESC\n        LIMIT 20"
        ));
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
    fn the_calling_frame_is_looked_for_under_the_contract_first() {
        let lead = "0x00000000000000000000000000000000000000bb";
        let implementation = "0x00000000000000000000000000000000000000cc";
        let sql = FrameQuery { with_parent: true, parent_in: vec![lead.to_string(), implementation.to_string()], ..query() }.sql();
        assert!(sql.contains("COALESCE(\n               (SELECT jsonb_build_object('selector', "));
        assert!(sql.contains(&format!(
            "WHERE q.to_a = ANY (ARRAY[{lead}, {implementation}]) AND q.block_number = r.block_number AND q.tx_index = r.tx_index\n"
        )));
        // The second lookup is by position alone.
        assert!(sql.contains("q\n                WHERE q.block_number = r.block_number AND q.tx_index = r.tx_index\n"));
        assert!(sql.contains("AND q.vm_step_start = r.caller_vm_step_stack[1])) AS parent"));
    }

    #[test]
    fn a_count_groups_a_bounded_sample_and_reads_no_calldata() {
        let q = FrameQuery { filters: vec!["s.vm_step_start = 0".to_string()], ..query() };
        let sql = q.count_sql(&[By::Function, By::Status]);
        assert!(sql.contains(
            "SELECT CASE WHEN s.call_opcode IN ('CREATE', 'CREATE2') THEN NULL ELSE s.sel END AS k1, \
             CASE WHEN s.error IS NOT NULL THEN s.error::text WHEN s.committed THEN 'ok' ELSE 'reverted' END AS k2,"
        ));
        assert!(sql.contains(
            "selector(d.calldata) AS sel\n        FROM {{transaction_detail(duration='30d', only_committed=False)}} d\n        \
             WHERE d.to_a = 0x00000000000000000000000000000000000000aa\n        ORDER BY d.block_number DESC\n        LIMIT 20000\n    ) s\n    \
             WHERE s.vm_step_start = 0\n    GROUP BY 1, 2\n    ORDER BY calls DESC, 1, 2\n    LIMIT 20\n) g\nORDER BY g.calls DESC, g.k1, g.k2"
        ));
        assert!(!sql.contains("d.calldata,"));
        assert!(
            query()
                .count_sql(&[By::Caller])
                .contains("LIMIT 20000\n    ) s\n    GROUP BY 1\n    ORDER BY calls DESC, 1\n    LIMIT 20\n) g")
        );
    }

    #[test]
    fn count_keys_are_taken_once_and_the_lead_contract_is_not_one() {
        assert_eq!(count_keys(Side::To, &[By::Function, By::Caller, By::Function]).unwrap(), vec![By::Function, By::Caller]);
        assert!(
            matches!(count_keys(Side::To, &[By::Callee]), Err(Fail::Usage(m)) if m.contains("`calls` counts by function, caller or status"))
        );
        assert!(
            matches!(count_keys(Side::From, &[By::Caller]), Err(Fail::Usage(m)) if m.contains("`subcalls` counts by function, callee or status"))
        );
    }

    #[test]
    fn functions_that_share_a_name_show_their_selectors() {
        let names = function_names(&[[1, 0, 0, 0], [2, 0, 0, 0], [3, 0, 0, 0], [4, 0, 0, 0]], |s| match s[0] {
            1 | 2 => Some("swap".to_string()),
            3 => Some("mint".to_string()),
            _ => None,
        });
        assert_eq!(names[&[1, 0, 0, 0]], "swap 0x01000000");
        assert_eq!(names[&[2, 0, 0, 0]], "swap 0x02000000");
        assert_eq!(names[&[3, 0, 0, 0]], "mint");
        assert_eq!(names[&[4, 0, 0, 0]], "0x04000000");
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
