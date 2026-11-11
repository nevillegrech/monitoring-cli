//! Lookups that write their own SQL in a shape that is fast, and decode what comes back.
//!
//! Rows are fetched raw (calldata, topics, data) through an index, newest first, and decoded
//! here with the ABI: the database does no per-row decoding, and no signature can be wrong.

use std::collections::BTreeMap;

use alloy_json_abi::Function;
use alloy_primitives::{B256, hex};
use serde_json::{Value, json};

use crate::api::Session;
use crate::config;
use crate::contracts;
use crate::error::{Fail, Res};
use crate::output::Table;
use crate::run::{self, QueryOpts};
use crate::sig::{self, EventAbi};

pub fn address(text: &str) -> Res<String> {
    hex_of(text, 40).ok_or_else(|| Fail::Usage(format!("`{text}` is not an address (0x and 40 hex digits)")))
}

pub fn tx_hash(text: &str) -> Res<String> {
    hex_of(text, 64).ok_or_else(|| Fail::Usage(format!("`{text}` is not a transaction hash (0x and 64 hex digits)")))
}

pub fn hex_of(text: &str, digits: usize) -> Option<String> {
    let body = text.trim().strip_prefix("0x").or_else(|| text.trim().strip_prefix("0X"))?;
    (body.len() == digits && body.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("0x{}", body.to_ascii_lowercase()))
}

pub fn cell<'a>(table: &'a Table, row: &'a [Value], name: &str) -> &'a Value {
    let index = table.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    &row[index]
}

pub fn bytes_cell(table: &Table, row: &[Value], name: &str) -> Vec<u8> {
    cell(table, row, name).as_str().and_then(sig::unhex).unwrap_or_default()
}

/// `ok`, the frame's own error, or `reverted` for a frame undone by a revert further up.
pub fn frame_status(error: &Value, committed: &Value) -> Value {
    match (error.as_str(), committed.as_bool()) {
        (Some(error), _) => json!(error),
        (None, Some(false)) => json!("reverted"),
        _ => json!("ok"),
    }
}

// --- ABIs ---

/// The functions of a contract by selector, read through a proxy when it is one.
pub struct ContractAbi {
    pub functions: BTreeMap<[u8; 4], Function>,
    pub implementation: Option<String>,
}

pub fn contract_abi(session: &mut Session, chain: &str, address: &str) -> Res<ContractAbi> {
    let items = match session.get(&format!("/api/contract/{chain}/{address}/abi"), &[]) {
        Ok(items) => items,
        Err(Fail::Query(_)) => Value::Null,
        Err(e) => return Err(e),
    };
    let mut abi = ContractAbi { functions: BTreeMap::new(), implementation: None };
    for item in items.as_array().into_iter().flatten() {
        if let Some(function) = sig::function_from_api(&item["abi"]) {
            abi.functions.insert(function.selector().0, function);
        }
        if let Some(source) = item["address"].as_str().filter(|a| !a.eq_ignore_ascii_case(address)) {
            abi.implementation = Some(source.to_ascii_lowercase());
        }
    }
    Ok(abi)
}

/// Known functions for selectors that the contract's own ABI does not name.
pub fn functions_by_selector(session: &mut Session, selectors: &[[u8; 4]]) -> BTreeMap<[u8; 4], Function> {
    let mut found = BTreeMap::new();
    if selectors.is_empty() {
        return found;
    }
    let body = json!(selectors.iter().map(|s| format!("0x{}", hex::encode(s))).collect::<Vec<_>>());
    if let Ok(items) = session.post("/api/search/function", &[], &body) {
        for function in items.as_array().into_iter().flatten().filter_map(sig::function_from_api) {
            found.entry(function.selector().0).or_insert(function);
        }
    }
    found
}

fn events_by_topic(session: &mut Session, topics: &[B256]) -> BTreeMap<B256, EventAbi> {
    let mut found = BTreeMap::new();
    if topics.is_empty() {
        return found;
    }
    let body = json!(topics.iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>());
    if let Ok(items) = session.post("/api/search/event", &[], &body) {
        for abi in items.as_array().into_iter().flatten().filter_map(EventAbi::from_api) {
            found.entry(abi.event.selector()).or_insert(abi);
        }
    }
    found
}

/// What a failed call returned: `Error("…")`, `Panic(code)`, a custom error with its
/// arguments, or the bytes when the error is not known.
pub fn revert_reason(session: &mut Session, known: &mut BTreeMap<[u8; 4], Option<Function>>, returndata: &[u8]) -> String {
    let Some(selector) = returndata.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return if returndata.is_empty() { String::new() } else { format!("0x{}", hex::encode(returndata)) };
    };
    let error = known.entry(selector).or_insert_with(|| match selector {
        [0x08, 0xc3, 0x79, 0xa0] => sig::parse_function("Error(string)").ok(),
        [0x4e, 0x48, 0x7b, 0x71] => sig::parse_function("Panic(uint256)").ok(),
        _ => session
            .get("/api/search/error", &[("selector", format!("0x{}", hex::encode(selector)))])
            .ok()
            .and_then(|found| found.as_array().and_then(|items| items.iter().find_map(sig::function_from_api))),
    });
    match error.as_ref().and_then(|e| sig::decode_values(e, returndata).map(|values| format!("{}({values})", e.name))) {
        Some(reason) => reason,
        None => format!("0x{}", hex::encode(returndata)),
    }
}

pub fn any_of(column: &str, values: &[String]) -> String {
    match values {
        [one] => format!("{column} = {one}"),
        many => format!("{column} = ANY(ARRAY[{}])", many.join(", ")),
    }
}

// --- events ---

pub struct EventsArgs {
    pub address: String,
    pub event: Option<String>,
    /// Add the sender, gas limit, gas used and status of each event's transaction.
    pub with_tx: bool,
    pub raw: bool,
}

/// The transaction of a row of `alias`, as one JSON column. A correlated scalar subquery on the
/// primary key runs once per row that is output, so a lookup costs at most 500 key reads. For
/// a large set of rows the guide's form is the right one: a materialized CTE ordered by
/// `(block_number, tx_index)`, merge-joined to `outer_transaction` under the same block bound.
pub fn tx_info(alias: &str) -> String {
    format!(
        ",\n       (SELECT jsonb_build_object('from', encode(o.from_a, 'hex'), 'gas', o.gas::text, 'gas_used', o.gas_used::text, 'ok', o.status)\n        \
         FROM outer_transaction o\n        \
         WHERE o.block_number = {alias}.block_number AND o.tx_index = {alias}.tx_index) AS tx_info"
    )
}

/// The four transaction cells of a fetched row, in the order of `TX_NAMES`.
pub const TX_NAMES: [&str; 4] = ["tx_from", "tx_gas_limit", "tx_gas_used", "tx_status"];

pub fn tx_cells(fetched: &Table, row: &[Value]) -> Vec<Value> {
    let info = cell(fetched, row, "tx_info");
    vec![
        info["from"].as_str().map(|a| json!(format!("0x{a}"))).unwrap_or(Value::Null),
        info["gas"].clone(),
        info["gas_used"].clone(),
        match info["ok"].as_bool() {
            Some(true) => json!("ok"),
            Some(false) => json!("failed"),
            None => Value::Null,
        },
    ]
}

/// The topic0 values an event argument stands for. A bare name is resolved against what the
/// contract emitted lately: the search API knows too many events of any common name.
fn topics_of(session: &mut Session, emitter: &str, event: &str, opts: &QueryOpts) -> Res<Vec<B256>> {
    if let Some(topic) = hex_of(event, 64) {
        return Ok(vec![B256::from_slice(&sig::unhex(&topic).expect("checked hex"))]);
    }
    if event.contains('(') {
        return Ok(vec![sig::parse_event(event)?.selector()]);
    }
    let sample = format!(
        "SELECT DISTINCT s.topic0\n\
         FROM (SELECT l.topic0 FROM {{{{logs(duration='{}')}}}} l WHERE l.address = {emitter} ORDER BY l.block_number DESC LIMIT 300) s",
        opts.last
    );
    let sample_opts = QueryOpts { limit: run::MAX_LIMIT, offset: 0, ..opts.clone() };
    let emitted = run::query(session, &sample, &sample_opts)?.table;
    let recent: Vec<B256> = emitted
        .rows
        .iter()
        .filter_map(|r| r[0].as_str().and_then(sig::unhex).filter(|b| b.len() == 32).map(|b| B256::from_slice(&b)))
        .collect();
    let abis = events_by_topic(session, &recent);
    let mut topics: Vec<B256> = abis.values().filter(|a| a.event.name == event).map(|a| a.event.selector()).collect();
    if topics.is_empty() {
        topics = run::search_events(session, event)?.into_iter().filter(|a| a.event.name == event).map(|a| a.event.selector()).collect();
        topics.sort();
        topics.dedup();
        topics.truncate(40);
    }
    if topics.is_empty() {
        let mut seen: Vec<&str> = abis.values().map(|a| a.event.name.as_str()).collect();
        seen.sort();
        seen.dedup();
        let lately = if seen.is_empty() { String::new() } else { format!(" The contract lately emitted: {}.", seen.join(", ")) };
        return Err(Fail::Usage(format!(
            "no event named `{event}` is known.{lately} Give a signature such as `Transfer(address,address,uint256)`, or a 0x topic0."
        )));
    }
    Ok(topics)
}

pub fn events(session: &mut Session, args: &EventsArgs, opts: &QueryOpts) -> Res<Table> {
    let emitter = address(&args.address)?;
    let mut conditions = vec![format!("l.address = {emitter}")];
    if let Some(event) = &args.event {
        let topics: Vec<String> = topics_of(session, &emitter, event, opts)?.iter().map(|t| sig::bytea(t.as_slice())).collect();
        conditions.push(any_of("l.topic0", &topics));
    }
    let sql = format!(
        "SELECT tx_hash(r.block_number, r.tx_index) AS tx, r.block_number AS block, ts(r.block_number) AS time,\n       \
         r.topic0, r.topic1, r.topic2, r.topic3, r.data{}\n\
         FROM (\n    \
         SELECT l.block_number, l.tx_index, l.log_index, l.topic0, l.topic1, l.topic2, l.topic3, l.data\n    \
         FROM {{{{logs(duration='{}')}}}} l\n    \
         WHERE {}\n    \
         ORDER BY l.block_number DESC\n    \
         LIMIT {}\n\
         ) r\n\
         ORDER BY r.block_number DESC, r.log_index DESC",
        if args.with_tx { tx_info("r") } else { String::new() },
        opts.last,
        conditions.join("\n      AND "),
        opts.limit + opts.offset,
    );
    let fetched = run::query(session, &sql, opts)?.table;
    if args.raw {
        return Ok(fetched);
    }
    let topics_of_row = |row: &[Value]| -> Vec<B256> {
        ["topic0", "topic1", "topic2", "topic3"]
            .iter()
            .map_while(|name| cell(&fetched, row, name).as_str().and_then(sig::unhex).filter(|b| b.len() == 32))
            .map(|bytes| B256::from_slice(&bytes))
            .collect()
    };
    let mut distinct: Vec<B256> = fetched.rows.iter().filter_map(|r| topics_of_row(r).first().copied()).collect();
    distinct.sort();
    distinct.dedup();
    let abis = events_by_topic(session, &distinct);
    let mut names = vec!["tx", "block", "time", "event", "args"];
    if args.with_tx {
        names.extend(TX_NAMES);
    }
    let mut table = Table::new(&names);
    for column in table.columns.iter_mut().filter(|c| c.name.starts_with("tx_gas")) {
        column.ty = "numeric".to_string();
    }
    for row in &fetched.rows {
        let topics = topics_of_row(row);
        let data = bytes_cell(&fetched, row, "data");
        let abi = topics.first().and_then(|t| abis.get(t));
        let decoded = abi
            .and_then(|abi| abi.candidates(topics.len().saturating_sub(1)).iter().find_map(|event| sig::decode_log(event, &topics, &data)));
        let name = match (abi, topics.first()) {
            (Some(abi), _) => abi.event.name.clone(),
            (None, Some(topic)) => format!("{topic:#x}"),
            (None, None) => "anonymous".to_string(),
        };
        let decoded_args = decoded.unwrap_or_else(|| {
            let rest: Vec<String> = topics.iter().skip(1).map(|t| format!("{t:#x}")).collect();
            format!("topics=[{}], data=0x{}", rest.join(", "), hex::encode(&data))
        });
        let mut out: Vec<Value> = ["tx", "block", "time"].iter().map(|c| cell(&fetched, row, c).clone()).collect();
        out.push(json!(name));
        out.push(json!(decoded_args));
        if args.with_tx {
            out.extend(tx_cells(&fetched, row));
        }
        table.rows.push(out);
    }
    Ok(table)
}

// --- tx ---

/// Where a transaction sits and what it cost, from the API's transaction record.
pub fn transaction(session: &mut Session, chain: &str, hash: &str) -> Res<Value> {
    session.get(&format!("/api/transaction/{chain}/{hash}"), &[])
}

/// The call frames of one transaction, in execution order. Returns the header line too.
pub fn tx(session: &mut Session, hash: &str, with_args: bool, opts: &QueryOpts) -> Res<(String, Table)> {
    let hash = tx_hash(hash)?;
    let record = transaction(session, &opts.chain, &hash)?;
    let (Some(block), Some(index)) = (record["block_number"].as_i64(), record["tx_index"].as_i64()) else {
        return Err(Fail::Query(format!("transaction {hash} is not known on {}", opts.chain)));
    };
    // The primary key is (block_number, tx_index, vm_step_start): with both as constants this
    // reads one transaction's rows. Two steps, never a join from outer_transaction by hash.
    let sql = format!(
        "SELECT d.vm_step_start AS step, cardinality(d.caller_vm_step_stack) AS depth, d.call_opcode AS op,\n       \
         d.from_a, d.to_a, d.gas, d.gas_used, d.callvalue AS value, d.error, d.committed,\n       \
         {} AS calldata\n\
         FROM transaction_detail d\n\
         WHERE d.block_number = {block} AND d.tx_index = {index}\n\
         ORDER BY d.vm_step_start\n\
         LIMIT {}",
        if with_args { "d.calldata" } else { "substring(d.calldata FROM 1 FOR 4)" },
        opts.limit + opts.offset,
    );
    let fetched = run::query(session, &sql, opts)?.table;
    let selector_of = |row: &[Value]| -> Option<[u8; 4]> { bytes_cell(&fetched, row, "calldata").get(..4)?.try_into().ok() };
    let mut selectors: Vec<[u8; 4]> = fetched.rows.iter().filter_map(|r| selector_of(r)).collect();
    selectors.sort();
    selectors.dedup();
    let known = functions_by_selector(session, &selectors);
    let mut names = vec!["step", "depth", "op", "from", "to", "gas", "gas_used", "value", "status", "function"];
    if with_args {
        names.push("args");
    }
    let mut table = Table::new(&names);
    for i in 5..8 {
        table.columns[i].ty = "numeric".to_string();
    }
    for row in &fetched.rows {
        let calldata = bytes_cell(&fetched, row, "calldata");
        let function = selector_of(row).and_then(|s| known.get(&s));
        let mut out: Vec<Value> =
            ["step", "depth", "op", "from_a", "to_a", "gas", "gas_used", "value"].iter().map(|c| cell(&fetched, row, c).clone()).collect();
        out.push(frame_status(cell(&fetched, row, "error"), cell(&fetched, row, "committed")));
        out.push(match (function, calldata.len()) {
            (Some(function), _) => json!(sig::function_text(function)),
            (None, 0) => json!(""),
            (None, _) => json!(format!("0x{}", hex::encode(&calldata[..calldata.len().min(4)]))),
        });
        if with_args {
            let decoded = function.and_then(|f| sig::decode_call(f, &calldata));
            out.push(json!(decoded.unwrap_or_else(|| format!("0x{}", hex::encode(calldata.get(4..).unwrap_or_default())))));
        }
        table.rows.push(out);
    }
    let failed = fetched.rows.first().is_some_and(|r| !cell(&fetched, r, "error").is_null());
    let header = format!(
        "# tx {hash} block {block} index {index} from {} to {} gas_limit {} gas_used {} gas_refund {} gas_price {} {}",
        record["from_a"].as_str().unwrap_or("?"),
        record["to_a"].as_str().unwrap_or("(contract creation)"),
        record["gas"],
        record["gas_used"],
        record["gas_refund"],
        record["gas_price"],
        if failed { "FAILED" } else { "succeeded" },
    );
    Ok((header, table))
}

// --- replay ---

/// The same transaction executed under today's gas rules and under the next fork's. Two
/// tables: one row per run, then one row per call frame of each run.
pub fn replay(session: &mut Session, chain: &str, hash: &str) -> Res<(String, Table, Table)> {
    let hash = tx_hash(hash)?;
    let report = session.get_slow(&format!("/api/transaction/{chain}/{hash}/gas-replay"), &[], 180)?;
    let now = report["rules"].as_str().unwrap_or("current").to_string();
    let next = report["other"].as_str().unwrap_or("next").to_string();
    let runs = [
        ("check", now.clone(), "as mined"),
        ("own_limit", next.clone(), "the transaction's own gas limit"),
        ("needed", next.clone(), "gas limit lifted"),
    ];
    let mut summary = Table::new(&["rules", "run", "status", "reason", "gas_limit", "gas_used", "regular_gas", "state_gas"]);
    let mut frames = Table::new(&["rules", "run", "depth", "kind", "to", "gas", "state_gas"]);
    for (key, rules, label) in &runs {
        let run = &report[*key];
        if run.is_null() {
            continue;
        }
        summary.rows.push(vec![
            json!(rules),
            json!(label),
            run["status"].clone(),
            run["reason"].clone(),
            run["gas_limit"].clone(),
            run["gas_used"].clone(),
            run["regular_gas"].clone(),
            run["state_gas"].clone(),
        ]);
        for frame in run["frames"].as_array().into_iter().flatten() {
            frames.rows.push(vec![
                json!(rules),
                json!(label),
                frame["depth"].clone(),
                frame["kind"].clone(),
                frame["to"].clone(),
                frame["gas"].clone(),
                frame["state_gas"].clone(),
            ]);
        }
    }
    let mismatch = report["mismatch"].as_array().map(|m| !m.is_empty()).unwrap_or(false);
    let mut header = format!(
        "# replay of {hash} with revm {}: the {now} run {} the mined receipt",
        report["revm"].as_str().unwrap_or("?"),
        if report["verified"].as_bool() == Some(true) && !mismatch { "reproduces" } else { "DOES NOT reproduce" },
    );
    if mismatch {
        header.push_str(&format!("; differences: {}", report["mismatch"]));
    }
    if report["incomplete"].as_bool() == Some(true) {
        header.push_str("; the replay is INCOMPLETE (state was missing)");
    }
    Ok((header, summary, frames))
}

// --- state ---

fn is_zero(value: &Value) -> bool {
    value.as_str().is_none_or(|s| s.trim_start_matches("0x").bytes().all(|b| b == b'0'))
}

/// What one transaction changed: storage slots, balances, nonces, code.
pub fn state(session: &mut Session, chain: &str, hash: &str) -> Res<(String, Table)> {
    let hash = tx_hash(hash)?;
    let report = session.get_slow(&format!("/api/transaction/{chain}/{hash}/state"), &[], 180)?;
    let mut table = Table::new(&["address", "roles", "kind", "key", "variable", "before", "after", "change"]);
    let (mut new_slots, mut cleared, mut changed, mut created) = (0, 0, 0, 0);
    for account in report["accounts"].as_array().into_iter().flatten() {
        let address = account["address"].clone();
        let roles = json!(
            account["roles"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default()
        );
        let mut push = |kind: &str, key: Value, variable: Value, before: Value, after: Value, change: &str| {
            table.rows.push(vec![address.clone(), roles.clone(), json!(kind), key, variable, before, after, json!(change)]);
        };
        if account["created"].as_bool() == Some(true) {
            created += 1;
            push("account", Value::Null, Value::Null, Value::Null, account["code_size"].clone(), "created");
        }
        if account["destroyed"].as_bool() == Some(true) {
            push("account", Value::Null, Value::Null, Value::Null, Value::Null, "destroyed");
        }
        if account["code_changed"].as_bool() == Some(true) {
            push("code", Value::Null, Value::Null, Value::Null, account["code_size"].clone(), "changed");
        }
        for field in ["balance", "nonce"] {
            let (before, after) = (&account[format!("{field}_before")], &account[format!("{field}_after")]);
            if !before.is_null() || !after.is_null() {
                push(field, Value::Null, Value::Null, before.clone(), after.clone(), "changed");
            }
        }
        let decoded: Vec<&Value> = account["decoded"].as_array().into_iter().flatten().collect();
        for slot in account["raw"].as_array().into_iter().flatten() {
            let change = match (is_zero(&slot["before"]), is_zero(&slot["after"])) {
                (true, false) => {
                    new_slots += 1;
                    "new"
                }
                (false, true) => {
                    cleared += 1;
                    "cleared"
                }
                _ => {
                    changed += 1;
                    "changed"
                }
            };
            let variables: Vec<String> = decoded
                .iter()
                .filter(|d| d["slot"] == slot["slot"])
                .map(|d| {
                    format!(
                        "{}: {} -> {}",
                        d["variable"].as_str().unwrap_or("?"),
                        d["before"].as_str().unwrap_or("?"),
                        d["after"].as_str().unwrap_or("?")
                    )
                })
                .collect();
            push("storage", slot["slot"].clone(), json!(variables.join("; ")), slot["before"].clone(), slot["after"].clone(), change);
        }
    }
    let header = format!(
        "# state changes of {hash}: {} storage slots written ({new_slots} new, {cleared} cleared, {changed} changed), {created} accounts created",
        new_slots + cleared + changed
    );
    Ok((header, table))
}

// --- abi and sig ---

/// The functions of a contract. With verified source they are its ABI. Without, they are the
/// functions the decompiler found in the bytecode: the API names those whose signature it
/// knows, and the rest are listed by selector.
pub fn abi(session: &mut Session, target: &str, opts: &QueryOpts) -> Res<Table> {
    let target = address(target)?;
    let chain = &opts.chain;
    let abi = contract_abi(session, chain, &target)?;
    let code_at = abi.implementation.clone().unwrap_or_else(|| target.clone());
    let record = match session.get(&format!("/api/contract/{chain}/{code_at}"), &[]) {
        Ok(record) => record,
        Err(Fail::Query(_)) => Value::Null,
        Err(e) => return Err(e),
    };
    let unverified = record["has_source"].as_bool() == Some(false);
    let unnamed: Vec<[u8; 4]> = if unverified {
        let found = contracts::bytecode_selectors(session, &code_at, opts)?;
        found.into_iter().filter(|selector| !abi.functions.contains_key(selector)).collect()
    } else {
        Vec::new()
    };
    if abi.functions.is_empty() && unnamed.is_empty() {
        return Err(Fail::Query(format!("no ABI is known for {target} on {chain}")));
    }
    if let Some(implementation) = &abi.implementation {
        run::note(&format!("{target} is a proxy; this is the ABI of its implementation {implementation}"));
    }
    if unverified {
        run::note(&format!(
            "{code_at} has no verified source: these are the functions found in its bytecode, {} of {} with a known signature \
             (`code` prints the decompiled code)",
            abi.functions.len(),
            abi.functions.len() + unnamed.len()
        ));
    }
    let mut table = Table::new(&["selector", "function", "mutability"]);
    let mut functions: Vec<(&[u8; 4], &Function)> = abi.functions.iter().collect();
    functions.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    for (selector, function) in functions {
        table.rows.push(vec![
            json!(format!("0x{}", hex::encode(selector))),
            json!(sig::function_text(function)),
            json!(format!("{:?}", function.state_mutability).to_ascii_lowercase()),
        ]);
    }
    for selector in unnamed {
        table.rows.push(vec![json!(format!("0x{}", hex::encode(selector))), Value::Null, Value::Null]);
    }
    Ok(table)
}

/// Look up a name, a signature, a selector or a topic0 among the known functions and events.
pub fn signatures(session: &mut Session, query: &str) -> Res<Table> {
    let query = query.trim();
    let mut table = Table::new(&["kind", "hash", "signature"]);
    let by_hash = hex_of(query, 8).or_else(|| hex_of(query, 64));
    let key = if by_hash.is_some() { "selector" } else { "signature" };
    let term = by_hash.clone().unwrap_or_else(|| query.to_string());
    // A signature alone does not say whether it is a function's or an event's, unless it uses
    // a word that only one of them has.
    let (only_event, only_function) = (query.contains(" indexed"), query.contains("returns"));
    if query.contains('(') {
        if let Some(function) = sig::parse_function(query).ok().filter(|_| !only_event) {
            table.rows.push(vec![
                json!("function (computed)"),
                json!(format!("{:#x}", function.selector())),
                json!(sig::function_text(&function)),
            ]);
        }
        if let Some(event) = sig::parse_event(query).ok().filter(|_| !only_function) {
            table.rows.push(vec![json!("event (computed)"), json!(format!("{:#x}", event.selector())), json!(sig::event_text(&event))]);
        }
    }
    // A search term is a name or a canonical signature; names and `indexed` do not belong in it.
    let term = if query.contains('(') {
        sig::parse_event(query).map(|e| e.signature()).or_else(|_| sig::parse_function(query).map(|f| f.signature())).unwrap_or(term)
    } else {
        term
    };
    // The search matches loosely; a full signature asks for that signature only.
    let exact = query.contains('(').then(|| term.clone());
    if by_hash.as_ref().is_none_or(|h| h.len() == 10) && !only_event {
        for path in ["/api/search/function", "/api/search/error"] {
            let kind = if path.ends_with("error") { "error" } else { "function" };
            let found = session.get(path, &[(key, term.clone())]).unwrap_or(Value::Null);
            let functions = found.as_array().into_iter().flatten().filter_map(sig::function_from_api);
            for function in functions.filter(|f| exact.as_ref().is_none_or(|e| f.signature() == *e)).take(25) {
                table.rows.push(vec![json!(kind), json!(format!("{:#x}", function.selector())), json!(sig::function_text(&function))]);
            }
        }
    }
    if by_hash.as_ref().is_none_or(|h| h.len() == 66) && !only_function {
        let found = session.get("/api/search/event", &[(key, term.clone())]).unwrap_or(Value::Null);
        let events = found.as_array().into_iter().flatten().filter_map(EventAbi::from_api);
        for abi in events.filter(|a| exact.as_ref().is_none_or(|e| a.event.signature() == *e)).take(25) {
            for (i, layout) in abi.layouts.iter().enumerate() {
                let kind = if i == 0 { "event" } else { "event (other indexed layout)" };
                table.rows.push(vec![
                    json!(kind),
                    json!(format!("{:#x}", abi.event.selector())),
                    json!(sig::event_text(&abi.with_layout(layout))),
                ]);
            }
        }
    }
    if table.rows.is_empty() {
        return Err(Fail::Query(format!("nothing is known under `{query}`")));
    }
    Ok(table)
}

// --- schema ---

const SCHEMA_MAX_AGE: u64 = 24 * 3600;

fn schema_json(session: &mut Session) -> Res<Value> {
    let path = config::state_dir().join("schema.json");
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < SCHEMA_MAX_AGE);
    if fresh && let Some(cached) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        return Ok(cached);
    }
    let schema = session.get_slow("/api/tsql/schema", &[], 180)?;
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(&path, schema.to_string());
    Ok(schema)
}

/// Table names of a chain, or the columns and indexes of the tables whose name contains `table`.
pub fn schema(session: &mut Session, chain: &str, table: Option<&str>) -> Res<String> {
    let schema = schema_json(session)?;
    let mut out = String::new();
    for namespace in [chain, "common"] {
        let mut tables: Vec<&Value> = schema[namespace].as_array().into_iter().flatten().collect();
        tables.sort_by_key(|t| t["name"].as_str().unwrap_or("").to_string());
        match table {
            None => {
                let names: Vec<&str> = tables.iter().filter_map(|t| t["name"].as_str()).collect();
                out.push_str(&format!("{namespace}: {}\n", names.join(" ")));
            }
            Some(wanted) => {
                let wanted = wanted.to_ascii_lowercase();
                let exact = tables.iter().any(|t| t["name"].as_str() == Some(&wanted));
                for t in tables.iter().filter(|t| {
                    let name = t["name"].as_str().unwrap_or("");
                    if exact { name == wanted } else { name.contains(&wanted) }
                }) {
                    out.push_str(&format!("{namespace}.{} ({})\n", t["name"].as_str().unwrap_or(""), t["type"].as_str().unwrap_or("")));
                    for column in t["columns"].as_array().into_iter().flatten() {
                        out.push_str(&format!(
                            "  {} {}\n",
                            column["column_name"].as_str().unwrap_or(""),
                            column["column_type"].as_str().unwrap_or("")
                        ));
                    }
                    for index in t["indices"].as_array().into_iter().flatten() {
                        let definition = index["index_definition"].as_str().unwrap_or("");
                        let unique = if definition.starts_with("CREATE UNIQUE") { "unique " } else { "" };
                        let columns = definition.split_once(" USING ").map(|(_, rest)| rest).unwrap_or(definition);
                        out.push_str(&format!("  {unique}index {}\n", columns.split_whitespace().collect::<Vec<_>>().join(" ")));
                    }
                }
            }
        }
    }
    if out.is_empty() || (table.is_some() && !out.contains('\n')) {
        return Err(Fail::Query(format!("no table matches `{}` on {chain}", table.unwrap_or(""))));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_hashes_are_checked_and_lowercased() {
        assert_eq!(address("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap(), "0xdac17f958d2ee523a2206206994597c13d831ec7");
        assert!(address("dac17f958d2ee523a2206206994597c13d831ec7").is_err());
        assert!(tx_hash("0x1234").is_err());
    }

    #[test]
    fn one_value_compares_and_several_use_any() {
        assert_eq!(any_of("x", &["1".to_string()]), "x = 1");
        assert_eq!(any_of("x", &["1".to_string(), "2".to_string()]), "x = ANY(ARRAY[1, 2])");
    }

    #[test]
    fn a_frame_reports_its_own_error_or_a_revert_above_it() {
        assert_eq!(frame_status(&json!("ErrOutOfGas"), &json!(false)), json!("ErrOutOfGas"));
        assert_eq!(frame_status(&Value::Null, &json!(false)), json!("reverted"));
        assert_eq!(frame_status(&Value::Null, &json!(true)), json!("ok"));
    }

    #[test]
    fn zero_words_are_recognized() {
        assert!(is_zero(&json!("0x0000000000000000000000000000000000000000000000000000000000000000")));
        assert!(!is_zero(&json!("0x0001000000000000000000000000000000000000650100006501000012c9040a")));
        assert!(is_zero(&Value::Null));
    }
}
