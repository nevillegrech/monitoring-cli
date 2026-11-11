//! A contract's code, and the search for contracts.
//!
//! The code of one contract comes from the API in one call: the verified source when there is
//! one, the decompiled code otherwise. The search runs SQL over `decompiled_code`, which has
//! the functions found in every bytecode, verified or not. `code` (the verified sources) is
//! only ever read by `md5_bytecode`: the query role has a row-security policy on that table,
//! under which PostgreSQL uses no index for a search by name or by selector.

use serde_json::{Value, json};

use crate::api::Session;
use crate::error::{Fail, Res};
use crate::lookups::{address, hex_of};
use crate::output::Table;
use crate::run::{self, QueryOpts};
use crate::sig;

const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// The implementation a proxy forwards to, and the proxy's kind.
fn implementation(session: &mut Session, chain: &str, proxy: &str) -> Res<Option<(String, String)>> {
    let info = match session.get(&format!("/api/contract/{chain}/{proxy}/proxy"), &[]) {
        Ok(info) => info,
        Err(Fail::Query(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let target = info["implementation_address"].as_str().map(str::to_ascii_lowercase);
    Ok(target.filter(|t| t != ZERO_ADDRESS && t != proxy).map(|t| (t, info["proxy_type"].as_str().unwrap_or("proxy").to_string())))
}

/// The selectors of every function the decompiler found in a contract's bytecode.
pub fn bytecode_selectors(session: &mut Session, contract: &str, opts: &QueryOpts) -> Res<Vec<[u8; 4]>> {
    let sql = format!(
        "SELECT ARRAY(SELECT encode(s, 'hex') FROM unnest(d.array_function_selectors) s) AS selectors\n\
         FROM contracts c\n\
         JOIN decompiled_code d USING (md5_bytecode)\n\
         WHERE c.address = {contract}"
    );
    let found = run::query(session, &sql, &QueryOpts { limit: 1, offset: 0, ..opts.clone() })?.table;
    let selectors = found.rows.first().and_then(|row| row[0].as_array()).into_iter().flatten();
    Ok(selectors.filter_map(|s| sig::unhex(s.as_str()?)?.try_into().ok()).collect())
}

// --- code ---

pub struct CodeArgs {
    pub address: String,
    /// The decompiled code, also when the source is verified.
    pub decompiled: bool,
    /// Every source file, not only the ones with code in the contract.
    pub all: bool,
    /// List the source files instead of printing them.
    pub files: bool,
    /// Only this source file: its path, or the last parts of it.
    pub file: Option<String>,
    /// The proxy's own code, not its implementation's.
    pub proxy: bool,
}

pub struct Code {
    pub header: String,
    /// One row per file: `path`, `main`, `in_contract`, `chars`, and `source` unless this is a listing.
    pub table: Table,
    pub listing: bool,
}

impl Code {
    /// The code as text: one file as it is, several each under a line that names its path.
    pub fn text(&self) -> String {
        let source = self.table.columns.len() - 1;
        let mut text = String::new();
        for row in &self.table.rows {
            if self.table.rows.len() > 1 {
                text.push_str(&format!("// ==== {} ====\n", row[0].as_str().unwrap_or_default()));
            }
            text.push_str(row[source].as_str().unwrap_or_default());
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        text
    }
}

struct SourceFile {
    path: String,
    main: bool,
    /// The file has code in this contract's bytecode. An interface or a linked library has none.
    in_contract: bool,
    source: String,
}

/// The source files of the API's `source_level`, the main file first.
fn source_files(code: &Value) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = code["source_level"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(path, file)| SourceFile {
            path: path.clone(),
            main: file["main"].as_bool().unwrap_or(false),
            in_contract: file["referenced"].as_bool().unwrap_or(false),
            source: file["source"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    files.sort_by(|a, b| b.main.cmp(&a.main).then_with(|| a.path.cmp(&b.path)));
    files
}

/// The files to print: the one named by `--file`, every file with `--all`, and otherwise those
/// with code in the contract (every file when none is marked).
fn select(files: Vec<SourceFile>, wanted: Option<&str>, all: bool) -> Res<Vec<SourceFile>> {
    let Some(wanted) = wanted else {
        let marked = files.iter().any(|f| f.in_contract);
        return Ok(files.into_iter().filter(|f| all || !marked || f.main || f.in_contract).collect());
    };
    // A whole path, or its last components: `Gateway.sol` is `src/Gateway.sol`, not `src/IGateway.sol`.
    let exact = files.iter().any(|f| f.path == wanted);
    let ends = |path: &str| path.strip_suffix(wanted).is_some_and(|rest| rest.ends_with('/'));
    let found: Vec<SourceFile> = files.into_iter().filter(|f| f.path == wanted || (!exact && ends(&f.path))).collect();
    match found.len() {
        1 => Ok(found),
        0 => Err(Fail::Usage(format!("no source file is named `{wanted}`; --files lists them"))),
        n => {
            let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
            Err(Fail::Usage(format!("`{wanted}` names {n} files: {}", paths.join(", "))))
        }
    }
}

fn file_table(files: &[SourceFile], with_source: bool) -> Table {
    let names = ["path", "main", "in_contract", "chars", "source"];
    let mut table = Table::new(if with_source { &names } else { &names[..4] });
    for file in files {
        let mut row = vec![json!(file.path), json!(file.main), json!(file.in_contract), json!(file.source.chars().count())];
        if with_source {
            row.push(json!(file.source));
        }
        table.rows.push(row);
    }
    table
}

fn files_text(count: usize) -> String {
    format!("{count} source file{}", if count == 1 { "" } else { "s" })
}

/// The verified source of a contract, or its decompiled code when there is none.
pub fn code(session: &mut Session, chain: &str, args: &CodeArgs) -> Res<Code> {
    let asked = address(&args.address)?;
    let mut target = asked.clone();
    if !args.proxy
        && let Some((implementation, kind)) = implementation(session, chain, &asked)?
    {
        run::note(&format!(
            "{asked} is a proxy ({kind}); this is the code of its implementation {implementation} (--proxy gives the proxy's own code)"
        ));
        target = implementation;
    }
    let path = format!("/api/contract/{chain}/{target}/code");
    let mut fetch = |fields: &[&str]| {
        let query: Vec<(&str, String)> = fields.iter().map(|field| ("code_fields", field.to_string())).collect();
        session.get_slow(&path, &query, 180)
    };
    if !args.decompiled {
        let code = fetch(&["source_level", "compiler", "compiled_contract_name"])?;
        let files = source_files(&code);
        if !files.is_empty() {
            let total = files.len();
            let about = format!(
                "# {target} {}, compiler {}",
                code["compiled_contract_name"].as_str().unwrap_or("(no name)"),
                code["compiler"].as_str().unwrap_or("unknown")
            );
            if args.files {
                let header = format!("{about}: {}; in_contract marks the files with code in this contract", files_text(total));
                return Ok(Code { header, table: file_table(&files, false), listing: true });
            }
            let shown = select(files, args.file.as_deref(), args.all)?;
            let header = if shown.len() == total {
                format!("{about}: {}", files_text(total))
            } else if args.file.is_some() {
                format!("{about}: 1 of {}", files_text(total))
            } else {
                format!(
                    "{about}: {} of {}, those with code in this contract (--all prints every file, --files lists them)",
                    shown.len(),
                    files_text(total)
                )
            };
            return Ok(Code { header, table: file_table(&shown, true), listing: false });
        }
        if args.files || args.all || args.file.is_some() {
            return Err(Fail::Query(format!(
                "{target} has no verified source on {chain}; without --files, --all and --file this prints its decompiled code"
            )));
        }
    }
    let code = fetch(&["decompiled_source_level"])?;
    let Some(text) = code["decompiled_source_level"].as_str().filter(|text| !text.trim().is_empty()) else {
        let what = if args.decompiled { "decompiled code" } else { "verified source or decompiled code" };
        return Err(Fail::Query(format!("no {what} is known for {target} on {chain}")));
    };
    let file = SourceFile { path: "decompiled".to_string(), main: true, in_contract: true, source: text.to_string() };
    let why = if args.decompiled { "" } else { " (it has no verified source)" };
    Ok(Code { header: format!("# {target}: decompiled code{why}"), table: file_table(&[file], true), listing: false })
}

// --- search ---

pub struct SearchArgs {
    /// Functions the bytecode must have: signatures or 0x selectors.
    pub functions: Vec<String>,
    pub name: Option<String>,
    pub deployer: Option<String>,
    pub same_code: Option<String>,
    pub no_source: bool,
    /// Only contracts created in this window.
    pub last: Option<String>,
}

const SEARCH_COLUMNS: [&str; 12] =
    ["address", "block", "time", "tx", "deployer", "created_by", "name", "protocol", "verified", "functions", "code", "destroyed"];

/// The platform's name search returns this many contracts and no more.
const NAME_SEARCH_MAX: u32 = 30;

fn selector_of(function: &str) -> Res<[u8; 4]> {
    if let Some(selector) = hex_of(function, 8) {
        return Ok(sig::unhex(&selector).and_then(|bytes| bytes.try_into().ok()).expect("checked hex"));
    }
    if !function.contains('(') {
        return Err(Fail::Usage(format!(
            "`{function}` is a name, and many functions share a name: give a signature such as `deposit(uint256,address)` \
             or a 0x selector (`dedaub-monitoring sig {function}` lists the known ones)"
        )));
    }
    Ok(sig::parse_function(function)?.selector().0)
}

/// A window such as `24h` or `30 day`, as PostgreSQL reads an interval.
fn window(text: &str) -> Res<&str> {
    let text = text.trim();
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let unit = text[digits..].trim_start();
    if digits == 0 || unit.is_empty() || !unit.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(Fail::Usage(format!("`{text}` is not a window such as 24h or 30d")));
    }
    Ok(text)
}

/// Seconds since 1970 as `2026-11-11T13:02:59Z`, the form `ts()` prints.
fn iso_time(seconds: i64) -> String {
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // The civil date of a day count, after Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// The search as SQL. The contracts are limited first, newest first; names are read after
/// that, one key lookup per row that is output.
fn search_sql(
    selectors: &[[u8; 4]],
    deployer: Option<&str>,
    code_id: Option<&str>,
    no_source: bool,
    last: Option<&str>,
    rows: u32,
) -> String {
    let mut conditions = Vec::new();
    if !selectors.is_empty() {
        let list: Vec<String> = selectors.iter().map(|s| sig::bytea(s)).collect();
        conditions.push(format!("d.array_function_selectors @> ARRAY[{}]::selector4[]", list.join(", ")));
    }
    if let Some(deployer) = deployer {
        conditions.push(format!("(c.eoa_deployer = {deployer} OR c.deployer = {deployer})"));
    }
    if let Some(code_id) = code_id {
        conditions.push(format!("c.md5_bytecode = {code_id}"));
    }
    if no_source {
        conditions.push("NOT EXISTS (SELECT 1 FROM code k WHERE k.md5_bytecode = c.md5_bytecode)".to_string());
    }
    if let Some(last) = last {
        conditions.push(format!("c.block_number > get_historical_block_number('{last}')"));
    }
    // With functions, a contract must have decompiled code; without, it is only decoration.
    let from = if selectors.is_empty() {
        "contracts c\n    LEFT JOIN decompiled_code d USING (md5_bytecode)"
    } else {
        "decompiled_code d\n    JOIN contracts c USING (md5_bytecode)"
    };
    // No condition at all is the newest contracts.
    let filter = if conditions.is_empty() { String::new() } else { format!("\n    WHERE {}", conditions.join("\n      AND ")) };
    format!(
        "SELECT r.address, r.block_number AS block, ts(r.block_number) AS time, tx_hash(r.block_number, r.tx_index) AS tx,\n       \
         r.eoa_deployer AS deployer, r.deployer AS created_by,\n       \
         (SELECT k.contract_name FROM code k WHERE k.md5_bytecode = r.md5_bytecode) AS name,\n       \
         (SELECT p.protocol_name FROM protocol_contract pc JOIN protocol p USING (protocol_id)\n        \
         WHERE pc.address = r.address LIMIT 1) AS protocol,\n       \
         EXISTS (SELECT 1 FROM code k WHERE k.md5_bytecode = r.md5_bytecode) AS verified,\n       \
         r.functions, r.md5_bytecode AS code, r.block_number_destroyed IS NOT NULL AS destroyed\n\
         FROM (\n    \
         SELECT c.address, c.block_number, c.tx_index, c.eoa_deployer, c.deployer, c.md5_bytecode, c.block_number_destroyed,\n           \
         cardinality(d.array_function_selectors) AS functions\n    \
         FROM {from}{filter}\n    \
         ORDER BY c.block_number DESC\n    \
         LIMIT {rows}\n\
         ) r\n\
         ORDER BY r.block_number DESC, r.address"
    )
}

/// Contracts by name, as the platform's own search ranks and names them.
fn by_name(session: &mut Session, name: &str, opts: &QueryOpts) -> Res<Table> {
    let found = session.get(&format!("/api/search/{}/contracts", opts.chain), &[("contract_name", name.to_string())])?;
    let mut table = Table::new(&SEARCH_COLUMNS);
    for item in found.as_array().into_iter().flatten().skip(opts.offset as usize).take(opts.limit as usize) {
        table.rows.push(vec![
            item["address"].clone(),
            item["block_number"].clone(),
            item["block_timestamp"].as_i64().map(|seconds| json!(iso_time(seconds))).unwrap_or(Value::Null),
            item["create_tx"].clone(),
            item["deployer"].clone(),
            Value::Null,
            item["name"].clone(),
            item["protocol_name"].clone(),
            item["has_source"].clone(),
            Value::Null,
            item["md5_bytecode"].clone(),
            json!(!item["block_number_destroyed"].is_null()),
        ]);
    }
    if opts.offset + opts.limit > NAME_SEARCH_MAX {
        run::note(&format!("the name search returns its {NAME_SEARCH_MAX} best matches and no more"));
    }
    Ok(table)
}

/// Contracts by the functions in their bytecode, their deployer or their code, newest first;
/// or by name.
pub fn search(session: &mut Session, args: &SearchArgs, opts: &QueryOpts) -> Res<Table> {
    if let Some(name) = &args.name {
        let alone =
            args.functions.is_empty() && args.deployer.is_none() && args.same_code.is_none() && !args.no_source && args.last.is_none();
        if !alone {
            return Err(Fail::Usage(
                "--name asks the platform's name search and takes no other condition: search by name, then filter the rows".to_string(),
            ));
        }
        return by_name(session, name.trim(), opts);
    }
    let selectors = args.functions.iter().map(|f| selector_of(f)).collect::<Res<Vec<_>>>()?;
    let deployer = args.deployer.as_deref().map(address).transpose()?;
    let last = args.last.as_deref().map(window).transpose()?;
    // The code id is read first and sent as a constant: the planner then knows how many
    // contracts share it, and does not walk every contract looking for them.
    let code_id = match &args.same_code {
        None => None,
        Some(contract) => {
            let contract = address(contract)?;
            let record = session.get(&format!("/api/contract/{}/{contract}", opts.chain), &[])?;
            let id = record["md5_bytecode"].as_str().and_then(|id| hex_of(id, 32));
            Some(id.ok_or_else(|| Fail::Query(format!("{contract} has no code on {}", opts.chain)))?)
        }
    };
    let sql = search_sql(&selectors, deployer.as_deref(), code_id.as_deref(), args.no_source, last, opts.limit + opts.offset);
    match run::query(session, &sql, opts) {
        Err(Fail::Timeout(_)) if !selectors.is_empty() => Err(Fail::Timeout(format!(
            "no result after {} s; the search was revoked. Functions that few contracts have are found by reading every \
             decompiled contract, which is slow the first time: run it again, add --last 30d to look at new contracts only, \
             or raise --timeout.",
            opts.timeout
        ))),
        other => Ok(other?.table),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, main: bool, in_contract: bool) -> SourceFile {
        SourceFile { path: path.to_string(), main, in_contract, source: format!("// {path}") }
    }

    fn paths(files: &[SourceFile]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn source_files_come_main_first_then_by_path() {
        let code = json!({"source_level": {
            "src/b.sol": {"main": false, "referenced": true, "source": "b"},
            "src/Main.sol": {"main": true, "referenced": true, "source": "m"},
            "lib/a.sol": {"main": false, "referenced": false, "source": "a"},
        }});
        let files = source_files(&code);
        assert_eq!(paths(&files), ["src/Main.sol", "lib/a.sol", "src/b.sol"]);
        assert!(!files[1].in_contract);
        assert!(source_files(&json!({"source_level": null})).is_empty());
    }

    #[test]
    fn the_default_is_the_files_with_code_in_the_contract() {
        let files = || vec![file("src/Main.sol", true, true), file("lib/I.sol", false, false), file("src/L.sol", false, true)];
        assert_eq!(paths(&select(files(), None, false).unwrap()), ["src/Main.sol", "src/L.sol"]);
        assert_eq!(select(files(), None, true).unwrap().len(), 3);
        // Nothing is marked: the marks are missing, not all false.
        let unmarked = vec![file("a.sol", true, false), file("b.sol", false, false)];
        assert_eq!(select(unmarked, None, false).unwrap().len(), 2);
    }

    #[test]
    fn one_file_is_picked_by_its_path_or_the_last_parts_of_it() {
        let files =
            || vec![file("src/v1/Calls.sol", false, false), file("src/v2/Calls.sol", false, true), file("src/ICalls.sol", true, true)];
        assert_eq!(paths(&select(files(), Some("v2/Calls.sol"), false).unwrap()), ["src/v2/Calls.sol"]);
        assert_eq!(paths(&select(files(), Some("ICalls.sol"), false).unwrap()), ["src/ICalls.sol"]);
        assert!(
            matches!(select(files(), Some("Calls.sol"), false), Err(Fail::Usage(m)) if m.contains("2 files: src/v1/Calls.sol, src/v2/Calls.sol"))
        );
        assert!(matches!(select(files(), Some("alls.sol"), false), Err(Fail::Usage(m)) if m.contains("--files")));
        // A whole path wins over the paths that end with it.
        let nested = vec![file("Calls.sol", true, true), file("src/Calls.sol", false, true)];
        assert_eq!(paths(&select(nested, Some("Calls.sol"), false).unwrap()), ["Calls.sol"]);
    }

    #[test]
    fn several_files_are_printed_under_their_paths_and_one_as_it_is() {
        let many = Code {
            header: String::new(),
            table: file_table(&[file("a.sol", true, true), file("b.sol", false, true)], true),
            listing: false,
        };
        assert_eq!(many.text(), "// ==== a.sol ====\n// a.sol\n// ==== b.sol ====\n// b.sol\n");
        let one = Code { header: String::new(), table: file_table(&[file("a.sol", true, true)], true), listing: false };
        assert_eq!(one.text(), "// a.sol\n");
        assert_eq!(file_table(&[file("a.sol", true, true)], false).columns.len(), 4);
    }

    #[test]
    fn a_function_is_a_signature_or_a_selector() {
        assert_eq!(selector_of("transfer(address,uint256)").unwrap(), [0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(selector_of("0xA9059CBB").unwrap(), [0xa9, 0x05, 0x9c, 0xbb]);
        assert!(matches!(selector_of("transfer"), Err(Fail::Usage(m)) if m.contains("sig transfer")));
    }

    #[test]
    fn windows_are_a_number_and_a_unit() {
        assert_eq!(window(" 30d ").unwrap(), "30d");
        assert_eq!(window("2 day").unwrap(), "2 day");
        for bad in ["", "d", "30", "30d'); --", "1 day ago"] {
            assert!(window(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn times_print_as_ts_does() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_time(1_794_428_159), "2026-11-11T20:15:59Z");
        assert_eq!(iso_time(1_713_363_839), "2024-04-17T14:23:59Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn a_search_by_functions_starts_from_the_decompiled_code() {
        let sql = search_sql(&[[0xa9, 0x05, 0x9c, 0xbb], [0x70, 0xa0, 0x82, 0x31]], None, None, true, Some("30d"), 20);
        assert!(sql.contains("FROM decompiled_code d\n    JOIN contracts c USING (md5_bytecode)"));
        assert!(sql.contains("d.array_function_selectors @> ARRAY['\\xa9059cbb'::bytea, '\\x70a08231'::bytea]::selector4[]"));
        assert!(sql.contains("AND NOT EXISTS (SELECT 1 FROM code k WHERE k.md5_bytecode = c.md5_bytecode)"));
        assert!(sql.contains("AND c.block_number > get_historical_block_number('30d')"));
        assert!(sql.contains("ORDER BY c.block_number DESC\n    LIMIT 20\n) r"));
    }

    #[test]
    fn a_search_without_functions_starts_from_the_contracts() {
        let deployer = "0x4b8a782d4f03ffcb7ce1e95c5cfe5bfcb2c8e967";
        let sql = search_sql(&[], Some(deployer), Some("0xb6d555d1194d3a5e0bfbba73eab269ec"), false, None, 5);
        assert!(sql.contains("FROM contracts c\n    LEFT JOIN decompiled_code d USING (md5_bytecode)"));
        assert!(sql.contains(&format!("WHERE (c.eoa_deployer = {deployer} OR c.deployer = {deployer})")));
        assert!(sql.contains("AND c.md5_bytecode = 0xb6d555d1194d3a5e0bfbba73eab269ec"));
        assert!(!sql.contains("get_historical_block_number"));
        // No condition at all is the newest contracts: the join is followed by the order, with no WHERE.
        let newest = search_sql(&[], None, None, false, None, 5);
        assert!(newest.contains("LEFT JOIN decompiled_code d USING (md5_bytecode)\n    ORDER BY c.block_number DESC\n    LIMIT 5"));
    }
}
