//! dedaub-monitoring: query on-chain data on the Dedaub platform.

mod api;
mod auth;
mod config;
mod contracts;
mod coverage;
mod error;
mod frames;
mod lookups;
mod output;
mod rewrite;
mod run;
mod sig;
mod skill;
mod slots;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::api::{Session, Window};
use crate::config::{Config, Profile};
use crate::error::{Fail, Res};
use crate::output::Format;
use crate::run::QueryOpts;

const GUIDE: &str = include_str!("guide.md");
const GUIDE_CONTRACTS: &str = include_str!("guide_contracts.md");
const GUIDE_TOKENS: &str = include_str!("guide_tokens.md");
const GUIDE_INCIDENTS: &str = include_str!("guide_incidents.md");
const GUIDE_OLD_DATA: &str = include_str!("guide_old_data.md");

/// A page of the guide besides the first.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
enum Page {
    Contracts,
    Tokens,
    Incidents,
    OldData,
    All,
}

/// The guide is in pages so that each fits the output an agent's shell shows in one piece.
fn guide(page: Option<Page>) -> String {
    match page {
        None => GUIDE.to_string(),
        Some(Page::Contracts) => GUIDE_CONTRACTS.to_string(),
        Some(Page::Tokens) => GUIDE_TOKENS.to_string(),
        Some(Page::Incidents) => GUIDE_INCIDENTS.to_string(),
        Some(Page::OldData) => GUIDE_OLD_DATA.to_string(),
        Some(Page::All) => [GUIDE, GUIDE_CONTRACTS, GUIDE_TOKENS, GUIDE_INCIDENTS, GUIDE_OLD_DATA].join("\n"),
    }
}

/// Seconds a query may run unless `--timeout` says otherwise. An agent's shell gives up on a
/// command after 120 s: a query revoked before that gets to print what to change.
const DEFAULT_TIMEOUT: u64 = 100;

/// (name, chain id) of every chain the platform indexes. The name is the SQL schema.
const CHAINS: [(&str, u64); 11] = [
    ("ethereum", 1),
    ("optimism", 10),
    ("binance", 56),
    ("polygon", 137),
    ("fantom", 250),
    ("robinhood", 4663),
    ("arc", 5042),
    ("base", 8453),
    ("arbitrum", 42161),
    ("avalanche", 43114),
    ("blast", 81457),
];

#[derive(Parser)]
#[command(
    name = "dedaub-monitoring",
    version,
    about = "Query on-chain data on the Dedaub platform",
    after_help = "Run `dedaub-monitoring guide` for the tables, the macros and the rules that keep a query fast.\n\
                  Exit codes: 0 ok, 1 query or server error, 2 usage, 3 not logged in, 4 query slots busy, 5 timeout."
)]
struct Cli {
    /// Chain name or id (ethereum, 1, base, 8453, …)
    #[arg(short, long, global = true, env = "DEDAUB_CHAIN", default_value = "ethereum", help_heading = "Global options")]
    chain: String,
    /// Login profile
    #[arg(short, long, global = true, help_heading = "Global options")]
    profile: Option<String>,
    /// Output format: tsv has a header line and prints NULL as null; json is one object per row
    #[arg(short, long, global = true, value_enum, default_value = "tsv", help_heading = "Global options")]
    format: Format,
    /// Print the SQL that is sent to the server, on stderr
    #[arg(long, global = true, help_heading = "Global options")]
    show_sql: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Limits {
    /// Rows to return (above 500 they are fetched in pages, at most 5000)
    #[arg(long, default_value_t = 100)]
    limit: u32,
    /// Rows to skip, for the next page
    #[arg(long, default_value_t = 0)]
    offset: u32,
    /// Seconds the query may run before it is revoked
    #[arg(long, default_value_t = DEFAULT_TIMEOUT)]
    timeout: u64,
    /// Seconds to wait for a free query slot (the account runs 3 queries at a time)
    #[arg(long, default_value_t = 600)]
    wait: u64,
    /// Cut cells longer than this many characters (0 = never)
    #[arg(long, default_value_t = 0)]
    max_cell: usize,
}

#[derive(Args, Clone)]
struct Paging {
    /// Time window, e.g. 1h, 24h, 30d
    #[arg(long, alias = "duration", default_value = "30d")]
    last: String,
    /// Rows to return (above 500 they are fetched in pages, at most 5000)
    #[arg(long, default_value_t = 20)]
    limit: u32,
    /// Rows to skip, for the next page
    #[arg(long, default_value_t = 0)]
    offset: u32,
    /// Seconds the query may run before it is revoked
    #[arg(long, default_value_t = DEFAULT_TIMEOUT)]
    timeout: u64,
    /// Seconds to wait for a free query slot
    #[arg(long, default_value_t = 600)]
    wait: u64,
    /// Cut cells longer than this many characters (0 = never)
    #[arg(long, default_value_t = 400)]
    max_cell: usize,
}

#[derive(Args, Clone)]
struct CallColumns {
    /// Add what each call returned: its outputs, or the revert reason of a failed call
    #[arg(long)]
    returns: bool,
    /// Add each call's transaction: sender, gas limit, gas used, status
    #[arg(long)]
    tx: bool,
    /// How many of the newest calls the filters (--failed, --top-level, --from) and --count-by look at
    #[arg(long, default_value_t = 20000, value_name = "N")]
    scan: u32,
    /// The rows as queried, with calldata as hex
    #[arg(long)]
    raw: bool,
    /// Count the calls instead of listing them: per function, caller (callee for subcalls) or status, or several (function,status)
    #[arg(long, value_enum, value_delimiter = ',', value_name = "KEY", conflicts_with_all = ["returns", "tx", "raw"])]
    count_by: Vec<frames::By>,
}

#[derive(Args, Clone)]
struct SqlInput {
    /// The SQL; without it, --file or stdin is read
    // SQL may begin with a `-- comment`, which is not an option.
    #[arg(allow_hyphen_values = true)]
    sql: Option<String>,
    /// Read the SQL from this file
    #[arg(long, value_name = "PATH")]
    file: Option<std::path::PathBuf>,
    /// Window of macros that give no duration=, e.g. 1h, 24h, 30d
    #[arg(long, alias = "duration", default_value = "24h")]
    last: String,
    /// End of that window (ISO 8601) instead of now
    #[arg(long, value_name = "TIME")]
    until: Option<String>,
    /// Send the SQL exactly as written: no rewrites, numbers as the API's floats
    #[arg(long)]
    raw: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Calls to a contract, newest first: caller, gas given and used, status, decoded arguments
    Calls {
        /// The contract (a proxy's own address, not its implementation's)
        address: String,
        /// A function name from the contract's ABI, a signature, or a 0x selector
        function: Option<String>,
        /// Only calls made by this address
        #[arg(long, value_name = "ADDRESS")]
        from: Option<String>,
        /// Only calls that failed
        #[arg(long)]
        failed: bool,
        /// Only calls made by the transaction itself, not by another contract
        #[arg(long)]
        top_level: bool,
        #[command(flatten)]
        extra: CallColumns,
        #[command(flatten)]
        paging: Paging,
    },
    /// Calls made by a contract, newest first: callee, gas given and used, status, and whether the calling function went on
    Subcalls {
        /// The calling contract
        address: String,
        /// Only calls to this address
        #[arg(long, value_name = "ADDRESS")]
        to: Option<String>,
        /// Only calls that failed
        #[arg(long)]
        failed: bool,
        /// Only failed calls whose calling frame went on without an error
        #[arg(long)]
        tolerated: bool,
        #[command(flatten)]
        extra: CallColumns,
        #[command(flatten)]
        paging: Paging,
    },
    /// Events of a contract, newest first, decoded with the known ABI
    Events {
        /// The emitting contract
        address: String,
        /// An event name, a signature, or a 0x topic0
        event: Option<String>,
        /// Add each event's transaction: sender, gas limit, gas used, status
        #[arg(long)]
        tx: bool,
        /// The rows as queried, with topics and data as hex
        #[arg(long)]
        raw: bool,
        #[command(flatten)]
        paging: Paging,
    },
    /// The call frames of one transaction: gas given and used, status, function
    Tx {
        hash: String,
        /// Decode each call's arguments
        #[arg(long)]
        args: bool,
        /// Frames to skip (500 are shown at a time)
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Cut cells longer than this many characters (0 = never)
        #[arg(long, default_value_t = 400)]
        max_cell: usize,
    },
    /// Re-execute a transaction under today's gas rules and the next fork's: status and gas per run and frame
    Replay { hash: String },
    /// What a transaction changed: storage slots (new, cleared, changed), balances, nonces, code
    State {
        hash: String,
        /// Cut cells longer than this many characters (0 = never)
        #[arg(long, default_value_t = 400)]
        max_cell: usize,
    },
    /// The functions of a contract with their selectors (through a proxy; without verified source, those found in the bytecode)
    Abi { address: String },
    /// The code of a contract: its verified source, or its decompiled code when it has none
    Code {
        /// The contract; for a proxy, the code of its implementation is printed
        address: String,
        /// The decompiled code, also when the source is verified
        #[arg(long, conflicts_with_all = ["all", "files", "file"])]
        decompiled: bool,
        /// Every source file, not only those with code in the contract (interfaces, linked libraries)
        #[arg(long)]
        all: bool,
        /// List the source files instead of printing them
        #[arg(long, conflicts_with = "file")]
        files: bool,
        /// Print one source file, named by its path or by the last parts of it (Gateway.sol, v2/Calls.sol)
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// The proxy's own code, not its implementation's
        #[arg(long)]
        proxy: bool,
    },
    /// Contracts, newest first: those with given functions in their bytecode (verified or not), of a deployer, of a code, or by name
    Contracts {
        /// Functions the contract must have: signatures such as 'deposit(uint256,address)', or 0x selectors
        functions: Vec<String>,
        /// Contracts by name instead, through the platform's name search: its 30 best matches, and no other condition
        #[arg(long, value_name = "TEXT")]
        name: Option<String>,
        /// Only contracts deployed by this account, or created by this contract
        #[arg(long, value_name = "ADDRESS")]
        deployer: Option<String>,
        /// Only contracts with the code of this one: its bytecode, or one that differs only in constants (column `same_bytecode`)
        #[arg(long, value_name = "ADDRESS")]
        same_code: Option<String>,
        /// Only contracts without verified source
        #[arg(long)]
        no_source: bool,
        /// Only contracts created in this window, e.g. 24h, 30d
        #[arg(long, alias = "duration", value_name = "WINDOW")]
        last: Option<String>,
        /// Rows to return (above 500 they are fetched in pages, at most 5000)
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Rows to skip, for the next page
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Seconds the query may run before it is revoked
        #[arg(long, default_value_t = DEFAULT_TIMEOUT)]
        timeout: u64,
        /// Seconds to wait for a free query slot
        #[arg(long, default_value_t = 600)]
        wait: u64,
    },
    /// Look up a function or event: by name, signature, 0x selector or 0x topic0
    Sig { query: String },
    /// The tables of the chain; with a name, the columns and indexes of the matching tables
    Schema { table: Option<String> },
    /// The blocks each history table holds, from which block its rows are uncompressed, and what leads to rows in either part
    #[command(after_help = "`indexed` lists the indexes of the uncompressed blocks. Compressed blocks are stored in the one order \
                      `indexed_compressed` gives: a condition on its first column leads to rows there. A condition on a column \
                      of `filtered_compressed` skips the batches without the value but looks at every batch of the window, \
                      and any other condition reads every row. `dedaub-monitoring guide old-data` says what to do then.\n\
                      `address_selector`, on the chains that have it, lists transactions by the selectors and addresses \
                      they involve.\n\
                      The ranges are built into this version, as read on the day the first line names.")]
    Coverage,
    /// Run a query and print its rows
    Sql {
        #[command(flatten)]
        input: SqlInput,
        #[command(flatten)]
        limits: Limits,
    },
    /// Print a query as the database receives it: rewrites applied, macros expanded
    Expand {
        #[command(flatten)]
        input: SqlInput,
    },
    /// Print the PostgreSQL plan of a query without running it
    Explain {
        #[command(flatten)]
        input: SqlInput,
    },
    /// Print the query guide: lookups, tables, helpers, the rules that keep a query fast; or one of its other pages
    Guide {
        /// contracts (code and proxies), tokens (balances, transfers, USD), incidents (three queries for an incident),
        /// old-data (questions about compressed blocks), or all
        #[arg(value_enum)]
        page: Option<Page>,
    },
    /// Check the login: exit 0 when queries can run, 3 when not
    Status,
    /// Log in through the browser
    Login {
        #[arg(long, default_value = config::DEFAULT_BASE_URL)]
        base_url: String,
        #[arg(long, default_value = config::DEFAULT_OIDC_HOST)]
        oidc_host: String,
        #[arg(long, default_value = config::DEFAULT_CLIENT_ID)]
        client_id: String,
        #[arg(long, default_value = config::DEFAULT_REALM)]
        realm: String,
    },
    /// Remove the stored login and revoke it on the server
    Logout {
        /// Keep the server session: tokens exported with `token` stay valid
        #[arg(long)]
        local: bool,
    },
    /// Print the stored refresh token, for DEDAUB_MONITORING_REFRESH_TOKEN on a machine without a browser
    Token,
    /// Install the agent skill: for Claude Code and every other supported agent found here
    InstallSkill {
        /// An agent to install for: claude, codex, cursor, agents (repeatable)
        #[arg(short, long, value_name = "NAME")]
        agent: Vec<String>,
        /// Install for every supported agent
        #[arg(long)]
        all: bool,
    },
}

fn chain_name(given: &str) -> Res<String> {
    let wanted = given.trim().to_ascii_lowercase();
    let alias = match wanted.as_str() {
        "bnb" | "bsc" => "binance",
        "eth" | "mainnet" => "ethereum",
        other => other,
    };
    CHAINS.iter().find(|(name, id)| *name == alias || id.to_string() == alias).map(|(name, _)| name.to_string()).ok_or_else(|| {
        let known: Vec<String> = CHAINS.iter().map(|(name, id)| format!("{name} ({id})")).collect();
        Fail::Usage(format!("unknown chain `{given}`; known chains: {}", known.join(", ")))
    })
}

/// SQL from the argument, the file or stdin. An inherited stdin that never sends anything (a
/// command started by an agent) must not hang the command, so the wait for a first byte is short.
fn read_sql(input: &SqlInput) -> Res<String> {
    if let Some(sql) = &input.sql {
        return Ok(sql.clone());
    }
    if let Some(path) = &input.file {
        return std::fs::read_to_string(path).map_err(|e| Fail::Usage(format!("cannot read {}: {e}", path.display())));
    }
    let missing = || Fail::Usage("no SQL given: pass it as an argument, with --file, or on stdin".to_string());
    if std::io::stdin().is_terminal() {
        return Err(missing());
    }
    let started = Arc::new(AtomicBool::new(false));
    let (send, receive) = std::sync::mpsc::channel();
    let seen = started.clone();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let mut chunk = [0u8; 65536];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut chunk) {
            if n == 0 {
                break;
            }
            seen.store(true, Ordering::SeqCst);
            text.extend_from_slice(&chunk[..n]);
        }
        let _ = send.send(text);
    });
    let text = match receive.recv_timeout(Duration::from_secs(2)) {
        Ok(text) => text,
        Err(_) if started.load(Ordering::SeqCst) => receive.recv().map_err(|_| missing())?,
        Err(_) => return Err(missing()),
    };
    String::from_utf8(text).map_err(|_| Fail::Usage("the SQL on stdin is not UTF-8".to_string()))
}

fn checked_limit(limit: u32) -> Res<u32> {
    if limit == 0 || limit > run::MAX_ROWS {
        return Err(Fail::Usage(format!("--limit must be between 1 and {}; --offset gives the rows after those", run::MAX_ROWS)));
    }
    Ok(limit)
}

fn query_opts(cli: &Cli, input: &SqlInput, limits: &Limits) -> Res<QueryOpts> {
    checked_limit(limits.limit)?;
    Ok(QueryOpts {
        chain: chain_name(&cli.chain)?,
        last: input.last.clone(),
        until: input.until.clone(),
        limit: limits.limit,
        offset: limits.offset,
        timeout: limits.timeout,
        wait: limits.wait,
        show_sql: cli.show_sql,
        raw: input.raw,
    })
}

fn paging_opts(cli: &Cli, paging: &Paging) -> Res<QueryOpts> {
    checked_limit(paging.limit)?;
    Ok(QueryOpts {
        chain: chain_name(&cli.chain)?,
        last: paging.last.clone(),
        until: None,
        limit: paging.limit,
        offset: paging.offset,
        timeout: paging.timeout,
        wait: paging.wait,
        show_sql: cli.show_sql,
        raw: false,
    })
}

/// Options of a lookup that runs SQL without a window of its own.
fn lookup_opts(cli: &Cli, limit: u32, offset: u32, timeout: u64, wait: u64) -> Res<QueryOpts> {
    let paging = Paging { last: "24h".into(), limit, offset, timeout, wait, max_cell: 0 };
    paging_opts(cli, &paging)
}

/// A reader that stops early (`| head`) is not an error.
fn written(result: std::io::Result<()>) -> Res<()> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(Fail::Other(format!("cannot write the result: {e}"))),
        _ => Ok(()),
    }
}

fn print_table(cli: &Cli, table: &output::Table, max_cell: usize) -> Res<()> {
    let mut out = std::io::stdout().lock();
    written(table.write(cli.format, max_cell, &mut out).and_then(|()| out.flush()))
}

fn print_text(text: &str) -> Res<()> {
    let mut out = std::io::stdout().lock();
    written(out.write_all(text.as_bytes()).and_then(|()| out.flush()))
}

fn row_count(table: &output::Table, opts: &QueryOpts) -> String {
    let rows = table.rows.len();
    let more = if rows as u32 == opts.limit {
        format!(" (the limit: there may be more; --offset {} gives the next page)", opts.offset + opts.limit)
    } else {
        String::new()
    };
    format!("# {rows} row{}{more}", if rows == 1 { "" } else { "s" })
}

fn compile_only(cli: &Cli, input: &SqlInput, session: &mut Session, kind: &str) -> Res<()> {
    let limits = Limits { limit: 1, offset: 0, timeout: 0, wait: 0, max_cell: 0 };
    let opts = query_opts(cli, input, &limits)?;
    let rewritten = run::prepare(session, &read_sql(input)?, &opts)?;
    let window = Window { last: opts.last.clone(), until: opts.until.clone() };
    let answer = session.compile(kind, &rewritten.sql, &opts.chain, Some(&window))?;
    match &answer {
        serde_json::Value::String(text) => println!("{text}"),
        serde_json::Value::Array(lines) => {
            for line in lines {
                println!("{}", line.as_str().unwrap_or_default());
            }
        }
        other => println!("{other}"),
    }
    Ok(())
}

fn status(session: &mut Session) -> Res<()> {
    session.me()?;
    let query = session.scratch_query_id()?;
    let running = session.active_tasks()?.len();
    println!(
        "logged in as {} (profile {}, {}); scratch query {query}; {running} of {} query slots in use",
        session.username().unwrap_or("?"),
        session.profile_name,
        session.profile.base_url,
        slots::slot_count()
    );
    Ok(())
}

fn login(cli: &Cli, profile: Profile, interrupted: &AtomicBool) -> Res<()> {
    let http = api::http_client();
    let mut profile = profile;
    let flow = auth::start_device_flow(&http, &profile)?;
    println!("\nOpen this URL to authenticate:\n  {}\n\nWaiting for authentication...", flow.verification_uri_complete);
    let refresh_token = auth::poll_token(&http, &profile, &flow, interrupted)?;
    let kind = auth::token_type(&refresh_token);
    if kind.as_deref() != Some("Offline") {
        eprintln!(
            "warning: the server gave a {} token, not an offline token, so this login expires with the browser session. \
             Ask a Dedaub admin to give the {} client and your user the offline_access scope.",
            kind.as_deref().unwrap_or("unknown"),
            profile.client_id
        );
    }
    profile.refresh_token = Some(refresh_token);
    let name = cli.profile.clone().unwrap_or_else(|| config::DEFAULT_PROFILE.to_string());
    let mut config = Config::load()?.unwrap_or_else(|| Config { default: name.clone(), profiles: Default::default() });
    config.profiles.insert(name.clone(), profile);
    config.save()?;
    Session::forget_state(&name);
    println!("Logged in.");
    Ok(())
}

fn logout(cli: &Cli, local: bool) -> Res<()> {
    let Some(mut config) = Config::load()? else {
        println!("Not logged in.");
        return Ok(());
    };
    let name = cli.profile.clone().unwrap_or_else(|| config.default.clone());
    let Some(profile) = config.profiles.remove(&name) else {
        println!("Not logged in.");
        return Ok(());
    };
    if profile.refresh_token.is_some()
        && !local
        && let Err(e) = auth::revoke(&api::http_client(), &profile)
    {
        eprintln!("warning: could not revoke the token on the server: {e}");
    }
    config.save()?;
    Session::forget_state(&name);
    println!("Logged out.");
    Ok(())
}

fn token(cli: &Cli) -> Res<()> {
    let config = Config::load()?.ok_or_else(|| Fail::Auth(error::NOT_LOGGED_IN.to_string()))?;
    let name = cli.profile.clone().unwrap_or_else(|| config.default.clone());
    let token = config.profiles.get(&name).and_then(|p| p.refresh_token.clone());
    println!("{}", token.ok_or_else(|| Fail::Auth(error::NOT_LOGGED_IN.to_string()))?);
    Ok(())
}

fn dispatch(cli: &Cli, interrupted: Arc<AtomicBool>) -> Res<()> {
    let open = || Session::open(cli.profile.as_deref(), interrupted.clone());
    match &cli.command {
        Command::Guide { page } => print_text(&guide(*page)),
        Command::Login { base_url, oidc_host, client_id, realm } => {
            let profile = Profile {
                base_url: base_url.clone(),
                oidc_host: oidc_host.clone(),
                client_id: client_id.clone(),
                realm: realm.clone(),
                refresh_token: None,
            };
            login(cli, profile, &interrupted)
        }
        Command::Logout { local } => logout(cli, *local),
        Command::Token => token(cli),
        Command::InstallSkill { agent, all } => skill::install(agent, *all),
        Command::Status => status(&mut open()?),
        Command::Sql { input, limits } => {
            let opts = query_opts(cli, input, limits)?;
            let sql = read_sql(input)?;
            let outcome = run::query(&mut open()?, &sql, &opts)?;
            print_table(cli, &outcome.table, limits.max_cell)?;
            eprintln!("{}", run::summary(&outcome, &opts));
            Ok(())
        }
        Command::Calls { address, function, from, failed, top_level, extra, paging } => {
            let opts = paging_opts(cli, paging)?;
            let args = frames::CallsArgs {
                side: frames::Side::To,
                address: address.clone(),
                other: from.clone(),
                function: function.clone(),
                failed: *failed,
                tolerated: false,
                top_level: *top_level,
                returns: extra.returns,
                with_tx: extra.tx,
                scan: extra.scan,
                raw: extra.raw,
                count_by: extra.count_by.clone(),
            };
            let table = frames::calls(&mut open()?, &args, &opts)?;
            print_table(cli, &table, paging.max_cell)?;
            eprintln!("{}", row_count(&table, &opts));
            Ok(())
        }
        Command::Subcalls { address, to, failed, tolerated, extra, paging } => {
            let opts = paging_opts(cli, paging)?;
            let args = frames::CallsArgs {
                side: frames::Side::From,
                address: address.clone(),
                other: to.clone(),
                function: None,
                failed: *failed,
                tolerated: *tolerated,
                top_level: false,
                returns: extra.returns,
                with_tx: extra.tx,
                scan: extra.scan,
                raw: extra.raw,
                count_by: extra.count_by.clone(),
            };
            let table = frames::calls(&mut open()?, &args, &opts)?;
            print_table(cli, &table, paging.max_cell)?;
            eprintln!("{}", row_count(&table, &opts));
            Ok(())
        }
        Command::Events { address, event, tx, raw, paging } => {
            let opts = paging_opts(cli, paging)?;
            let args = lookups::EventsArgs { address: address.clone(), event: event.clone(), with_tx: *tx, raw: *raw };
            let table = lookups::events(&mut open()?, &args, &opts)?;
            print_table(cli, &table, paging.max_cell)?;
            eprintln!("{}", row_count(&table, &opts));
            Ok(())
        }
        Command::Tx { hash, args, offset, max_cell } => {
            let opts = lookup_opts(cli, run::MAX_LIMIT, *offset, DEFAULT_TIMEOUT, 600)?;
            let (header, table) = lookups::tx(&mut open()?, hash, *args, &opts)?;
            eprintln!("{header}");
            print_table(cli, &table, *max_cell)?;
            eprintln!("{}", row_count(&table, &opts));
            Ok(())
        }
        Command::Replay { hash } => {
            let (header, summary, frames) = lookups::replay(&mut open()?, &chain_name(&cli.chain)?, hash)?;
            eprintln!("{header}");
            print_table(cli, &summary, 0)?;
            println!();
            print_table(cli, &frames, 0)
        }
        Command::State { hash, max_cell } => {
            let (header, table) = lookups::state(&mut open()?, &chain_name(&cli.chain)?, hash)?;
            eprintln!("{header}");
            print_table(cli, &table, *max_cell)
        }
        Command::Abi { address } => {
            let table = lookups::abi(&mut open()?, address, &lookup_opts(cli, 1, 0, DEFAULT_TIMEOUT, 600)?)?;
            print_table(cli, &table, 0)
        }
        Command::Code { address, decompiled, all, files, file, proxy } => {
            let args = contracts::CodeArgs {
                address: address.clone(),
                decompiled: *decompiled,
                all: *all,
                files: *files,
                file: file.clone(),
                proxy: *proxy,
            };
            let code = contracts::code(&mut open()?, &chain_name(&cli.chain)?, &args)?;
            eprintln!("{}", code.header);
            // Code is text. As rows it is one file per row, for `--format json` and the listing.
            if code.listing || cli.format != Format::Tsv { print_table(cli, &code.table, 0) } else { print_text(&code.text()) }
        }
        Command::Contracts { functions, name, deployer, same_code, no_source, last, limit, offset, timeout, wait } => {
            let opts = lookup_opts(cli, *limit, *offset, *timeout, *wait)?;
            let args = contracts::SearchArgs {
                functions: functions.clone(),
                name: name.clone(),
                deployer: deployer.clone(),
                same_code: same_code.clone(),
                no_source: *no_source,
                last: last.clone(),
            };
            let table = contracts::search(&mut open()?, &args, &opts)?;
            print_table(cli, &table, 0)?;
            eprintln!("{}", row_count(&table, &opts));
            Ok(())
        }
        Command::Sig { query } => {
            let table = lookups::signatures(&mut open()?, query)?;
            print_table(cli, &table, 0)
        }
        Command::Schema { table } => print_text(&lookups::schema(&mut open()?, &chain_name(&cli.chain)?, table.as_deref())?),
        Command::Coverage => {
            let chain = chain_name(&cli.chain)?;
            let (table, notes) = coverage::table(&chain);
            eprintln!(
                "# {chain} as read on {}. Newer rows have arrived since, and older blocks get compressed, but never the last {} days.",
                coverage::read_on(),
                coverage::ALWAYS_UNCOMPRESSED_DAYS
            );
            for text in &notes {
                run::note(text);
            }
            print_table(cli, &table, 0)
        }
        Command::Expand { input } => compile_only(cli, input, &mut open()?, "preview"),
        Command::Explain { input } => compile_only(cli, input, &mut open()?, "explain"),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    match dispatch(&cli, interrupted) {
        Ok(()) => ExitCode::SUCCESS,
        Err(fail) => {
            eprintln!("error: {fail}");
            ExitCode::from(fail.exit_code() as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chains_by_name_id_and_alias() {
        assert_eq!(chain_name("ethereum").unwrap(), "ethereum");
        assert_eq!(chain_name("1").unwrap(), "ethereum");
        assert_eq!(chain_name("8453").unwrap(), "base");
        assert_eq!(chain_name("BNB").unwrap(), "binance");
        assert!(matches!(chain_name("solana"), Err(Fail::Usage(m)) if m.contains("arbitrum (42161)")));
    }

    #[test]
    fn the_command_line_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Command, String> {
        let mut line = vec!["dedaub-monitoring"];
        line.extend(args);
        Cli::try_parse_from(line).map(|cli| cli.command).map_err(|e| e.to_string())
    }

    #[test]
    fn every_page_of_the_guide_fits_a_shell_and_the_rules_come_early() {
        for page in [None, Some(Page::Contracts), Some(Page::Tokens), Some(Page::Incidents), Some(Page::OldData)] {
            assert!(guide(page).len() < 30_000, "{page:?} is longer than an agent's shell shows in one piece");
        }
        assert!(matches!(parse(&["guide"]), Ok(Command::Guide { page: None })));
        assert!(matches!(parse(&["guide", "incidents"]), Ok(Command::Guide { page: Some(Page::Incidents) })));
        assert!(matches!(parse(&["guide", "old-data"]), Ok(Command::Guide { page: Some(Page::OldData) })));
        // Agents read the first hundred or so lines: the rules and the helpers must be there.
        let head: Vec<&str> = GUIDE.lines().take(100).collect();
        for needle in
            ["## The rules that keep a query fast", "**8. Give the planner", "block_timestamp(block_number)", "### When a query fails"]
        {
            assert!(head.iter().any(|line| line.contains(needle)), "`{needle}` is not in the first 100 lines of the guide");
        }
        // The whole guide stays small: an example that a lookup or another page already gives is left out.
        let all = guide(Some(Page::All));
        assert!(all.len() < 33_000, "the guide has grown to {} bytes", all.len());
        for page in ["guide contracts", "guide tokens", "guide incidents", "guide old-data"] {
            assert!(head.iter().any(|line| line.contains(page)), "the first page does not name `{page}`");
        }
        assert!(guide(Some(Page::All)).contains("# dedaub-monitoring guide: an incident"));
        // What is sure about compression, and where the ranges are.
        let days = format!("the last {} days", coverage::ALWAYS_UNCOMPRESSED_DAYS);
        assert!(GUIDE.contains(&days) && GUIDE.contains("dedaub-monitoring coverage"));
        assert!(matches!(parse(&["coverage", "--chain", "base"]), Ok(Command::Coverage)));
    }

    #[test]
    fn the_page_on_old_data_reads_what_coverage_prints() {
        // The page holds for every chain: what differs between chains is read from `coverage`.
        for column in ["uncompressed_from_block", "indexed_compressed", "filtered_compressed", "indexed"] {
            assert!(coverage::COLUMNS.contains(&column) && GUIDE_OLD_DATA.contains(&format!("`{column}`")), "{column}");
        }
        assert!(GUIDE_OLD_DATA.contains(&format!("older than {} days", coverage::ALWAYS_UNCOMPRESSED_DAYS)));
        for needle in
            [coverage::SIDE_TABLE, "encode_adresses_selectors(ARRAY[", "INTERSECT", "subcalls 0xCONTRACT --to 0xCALLEE", "tx 0xHASH"]
        {
            assert!(GUIDE_OLD_DATA.contains(needle), "the page on old data lacks `{needle}`");
        }
        assert!(GUIDE.contains("`guide old-data` says how to ask"));
    }

    #[test]
    fn limits_above_one_request_are_taken_up_to_the_cap() {
        assert_eq!(checked_limit(500).unwrap(), 500);
        assert_eq!(checked_limit(run::MAX_ROWS).unwrap(), run::MAX_ROWS);
        assert!(matches!(checked_limit(run::MAX_ROWS + 1), Err(Fail::Usage(_))));
        assert!(matches!(checked_limit(0), Err(Fail::Usage(_))));
    }

    #[test]
    fn duration_is_another_word_for_last() {
        let address = "0x00000000000000000000000000000000000000aa";
        assert!(matches!(parse(&["calls", address, "--duration", "365d"]), Ok(Command::Calls { paging, .. }) if paging.last == "365d"));
        assert!(matches!(parse(&["events", address, "--last", "7d"]), Ok(Command::Events { paging, .. }) if paging.last == "7d"));
        assert!(matches!(parse(&["sql", "SELECT 1", "--duration", "2d"]), Ok(Command::Sql { input, .. }) if input.last == "2d"));
        assert!(
            matches!(parse(&["contracts", "--deployer", address, "--duration", "30d"]), Ok(Command::Contracts { last, .. }) if last.as_deref() == Some("30d"))
        );
    }

    #[test]
    fn a_query_is_revoked_before_a_shell_gives_up_on_it() {
        assert!(matches!(parse(&["sql", "SELECT 1"]), Ok(Command::Sql { limits, .. }) if limits.timeout < 120));
        assert!(matches!(parse(&["sql", "SELECT 1", "--timeout", "300"]), Ok(Command::Sql { limits, .. }) if limits.timeout == 300));
    }

    #[test]
    fn calls_are_counted_by_one_key_or_several() {
        use frames::By;
        let address = "0x00000000000000000000000000000000000000aa";
        assert!(
            matches!(parse(&["calls", address, "--count-by", "function,status"]), Ok(Command::Calls { extra, .. }) if extra.count_by == [By::Function, By::Status])
        );
        assert!(
            matches!(parse(&["subcalls", address, "--count-by", "callee"]), Ok(Command::Subcalls { extra, .. }) if extra.count_by == [By::Callee])
        );
        assert!(matches!(parse(&["calls", address]), Ok(Command::Calls { extra, .. }) if extra.count_by.is_empty()));
        assert!(parse(&["calls", address, "--count-by", "function", "--tx"]).is_err());
        assert!(parse(&["calls", address, "--count-by", "gas"]).is_err());
    }

    fn sql_of(args: &[&str]) -> Result<(Option<String>, u32), String> {
        let mut line = vec!["dedaub-monitoring"];
        line.extend(args);
        match Cli::try_parse_from(line).map_err(|e| e.to_string())?.command {
            Command::Sql { input, limits } => Ok((input.sql, limits.limit)),
            _ => Err("not the sql command".to_string()),
        }
    }

    #[test]
    fn sql_may_begin_with_a_comment_and_options_go_on_either_side() {
        let commented = "-- newest block\nSELECT 1";
        assert_eq!(sql_of(&["sql", commented]).unwrap(), (Some(commented.to_string()), 100));
        assert_eq!(sql_of(&["sql", "--limit", "5", commented]).unwrap(), (Some(commented.to_string()), 5));
        assert_eq!(sql_of(&["sql", "SELECT -1", "--limit", "5"]).unwrap(), (Some("SELECT -1".to_string()), 5));
        assert_eq!(sql_of(&["sql", "--file", "q.sql", "--limit", "7"]).unwrap(), (None, 7));
        // Global options after the command are still options, not SQL.
        assert_eq!(sql_of(&["sql", "--show-sql", "--chain", "base", "SELECT 1"]).unwrap(), (Some("SELECT 1".to_string()), 100));
        assert_eq!(sql_of(&["--format", "json", "sql", commented, "--show-sql"]).unwrap(), (Some(commented.to_string()), 100));
        assert!(sql_of(&["sql", "SELECT 1", "SELECT 2"]).is_err());
    }
}
