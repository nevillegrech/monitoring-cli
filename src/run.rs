//! Running one query: rewrite, compile, queue for a slot, execute, wait, read the rows.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::{self, Session, Window};
use crate::config;
use crate::error::{Fail, Res};
use crate::output::{Column, Table};
use crate::rewrite::{self, Abis, EventUse, Rewritten};
use crate::sig::{self, EventAbi};
use crate::slots;

/// The server's cap on `limit`.
pub const MAX_LIMIT: u32 = 500;

#[derive(Clone, Debug)]
pub struct QueryOpts {
    pub chain: String,
    /// The window of macros that name no `duration`.
    pub last: String,
    pub until: Option<String>,
    pub limit: u32,
    pub offset: u32,
    /// Seconds a query may run before it is revoked.
    pub timeout: u64,
    /// Seconds to wait for a free query slot.
    pub wait: u64,
    pub show_sql: bool,
    /// Send the SQL exactly as given.
    pub raw: bool,
}

pub struct Outcome {
    pub table: Table,
    pub seconds: f64,
    pub queued: f64,
}

/// Event ABIs from the search API, asked once per signature.
pub struct ApiAbis<'a> {
    pub session: &'a mut Session,
    seen: Vec<(String, Vec<EventAbi>)>,
}

impl<'a> ApiAbis<'a> {
    pub fn new(session: &'a mut Session) -> Self {
        Self { session, seen: Vec::new() }
    }
}

impl Abis for ApiAbis<'_> {
    fn events(&mut self, signature: &str) -> Res<Vec<EventAbi>> {
        if let Some((_, found)) = self.seen.iter().find(|(s, _)| s == signature) {
            return Ok(found.clone());
        }
        let found = search_events(self.session, signature)?;
        self.seen.push((signature.to_string(), found.clone()));
        Ok(found)
    }
}

pub fn search_events(session: &mut Session, signature_or_topic: &str) -> Res<Vec<EventAbi>> {
    let key = if signature_or_topic.starts_with("0x") { "selector" } else { "signature" };
    let found = session.get("/api/search/event", &[(key, signature_or_topic.to_string())])?;
    Ok(found.as_array().into_iter().flatten().filter_map(EventAbi::from_api).collect())
}

pub fn note(text: &str) {
    eprintln!("note: {text}");
}

/// Apply this tool's rewrites and print what they say.
pub fn prepare(session: &mut Session, sql: &str, opts: &QueryOpts) -> Res<Rewritten> {
    let rewritten = if opts.raw {
        Rewritten { sql: sql.trim().to_string(), ..Rewritten::default() }
    } else {
        rewrite::rewrite(sql, &mut ApiAbis::new(session))?
    };
    for text in &rewritten.notes {
        note(text);
    }
    if opts.show_sql {
        eprintln!("-- sent to the server (chain {}):\n{}\n--", opts.chain, rewritten.sql);
    }
    Ok(rewritten)
}

/// The result columns of a query, which also checks that it compiles.
pub fn columns(session: &mut Session, sql: &str, chain: &str) -> Res<Vec<Column>> {
    let described = session.compile("metadata", sql, chain, None)?;
    Ok(described
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Column {
            name: c["column_name"].as_str().unwrap_or("").to_string(),
            ty: c["column_type"].as_str().unwrap_or("").to_string(),
        })
        .collect())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The API sends `numeric` as a float: 230000.0 for a gas amount, and a rounded value for
/// anything above 2^53. Selecting such columns as text keeps every digit.
fn exact_numerics(sql: &str, columns: &[Column]) -> Option<String> {
    if !columns.iter().any(|c| c.ty == "numeric") {
        return None;
    }
    let unique = columns.iter().enumerate().all(|(i, c)| !c.name.is_empty() && columns[..i].iter().all(|d| d.name != c.name));
    if !unique {
        note(
            "two result columns share a name, so the API returns only one of them and numbers stay floats: give each column its own alias",
        );
        return None;
    }
    let list: Vec<String> = columns
        .iter()
        .map(|c| {
            let name = quote_ident(&c.name);
            if c.ty == "numeric" { format!("_q.{name}::text AS {name}") } else { format!("_q.{name}") }
        })
        .collect();
    Some(format!("SELECT {}\nFROM (\n{sql}\n) _q", list.join(", ")))
}

fn sleep_watching(session: &Session, duration: Duration) -> Res<()> {
    let end = Instant::now() + duration;
    loop {
        if session.interrupted.load(Ordering::SeqCst) {
            return Err(Fail::Interrupted);
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// Queue, execute and wait. Returns the rows and the seconds spent queueing.
fn execute(session: &mut Session, sql: &str, opts: &QueryOpts, limit: u32, offset: u32, timeout: u64) -> Res<(Vec<Value>, f64)> {
    let queued_at = Instant::now();
    let wait_until = queued_at + Duration::from_secs(opts.wait);
    let slot_dir = config::state_dir().join("slots").join(&session.profile_name);
    let slot = slots::acquire(&slot_dir, slots::slot_count(), wait_until, &session.interrupted)?;
    let window = Window { last: opts.last.clone(), until: opts.until.clone() };
    let mut backoff = Duration::from_millis(400);
    let mut recreated = false;
    let task = loop {
        match session.execute_async(sql, &opts.chain, &window, limit, offset) {
            Ok(Some(task)) => break task,
            Ok(None) => {
                if Instant::now() >= wait_until {
                    return Err(Fail::Busy(format!(
                        "the account's concurrent-query limit stayed reached for {} s (other machines and the web app share it); \
                         raise --wait or run fewer queries at once",
                        opts.wait
                    )));
                }
                sleep_watching(session, backoff)?;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
            Err(Fail::Query(message)) if message.contains("Folder not found") && !recreated => {
                recreated = true;
                session.forget_scratch();
            }
            Err(e) => return Err(e),
        }
    };
    let queued = queued_at.elapsed().as_secs_f64();
    let started = Instant::now();
    let mut pause = Duration::from_millis(150);
    let rows = loop {
        let status = match sleep_watching(session, pause).and_then(|()| session.task(&task)) {
            Ok(status) => status,
            Err(e) => {
                session.cancel(&task);
                return Err(e);
            }
        };
        match status["status"].as_str() {
            Some("SUCCESS") => break status["result"].as_array().cloned().unwrap_or_default(),
            Some("FAILURE") => {
                let message = status["error"].as_str().unwrap_or("the query failed");
                return Err(Fail::Query(api::clean(message)));
            }
            Some(state @ ("REVOKED" | "IGNORED")) => {
                return Err(Fail::Other(format!("the server ended the query ({state})")));
            }
            _ => {}
        }
        if started.elapsed() >= Duration::from_secs(timeout) {
            session.cancel(&task);
            return Err(Fail::Timeout(format!(
                "no result after {timeout} s; the query was revoked. Shorten the window first: `duration='2d'` in a macro, \
                 `block_number > get_historical_block_number('2 day')` on a bare table, `--last 2d` on a lookup. A filter, \
                 sort or aggregate reads every row of the window, and a join needs the bound on both tables. \
                 `dedaub-monitoring guide` has the rules; --timeout raises the limit."
            )));
        }
        pause = (pause * 3 / 2).min(Duration::from_secs(1));
    };
    drop(slot);
    Ok((rows, queued))
}

/// Run a query that is already rewritten.
pub fn run(session: &mut Session, rewritten: &Rewritten, opts: &QueryOpts) -> Res<Outcome> {
    let started = Instant::now();
    let (columns, sent) = if opts.raw {
        (Vec::new(), rewritten.sql.clone())
    } else {
        let columns = columns(session, &rewritten.sql, &opts.chain)?;
        let sent = exact_numerics(&rewritten.sql, &columns).unwrap_or_else(|| rewritten.sql.clone());
        (columns, sent)
    };
    let (rows, queued) = execute(session, &sent, opts, opts.limit, opts.offset, opts.timeout)?;
    let table = Table::from_objects(columns, rows);
    if table.rows.is_empty() && opts.offset == 0 {
        for event in rewritten.events.iter().take(2) {
            if let Some(text) = explain_no_rows(session, event, opts) {
                note(&text);
            }
        }
    }
    Ok(Outcome { table, seconds: started.elapsed().as_secs_f64(), queued })
}

pub fn query(session: &mut Session, sql: &str, opts: &QueryOpts) -> Res<Outcome> {
    let rewritten = prepare(session, sql, opts)?;
    run(session, &rewritten, opts)
}

/// Why did a query over an event return nothing? Look at one log with the event's topic0: the
/// number of topics it carries shows whether the `indexed` flags are the reason.
fn explain_no_rows(session: &mut Session, event: &EventUse, opts: &QueryOpts) -> Option<String> {
    let address = event.address.as_ref().map(|a| format!(" AND l.address = {a}")).unwrap_or_default();
    let probe = format!(
        "SELECT (l.topic1 IS NOT NULL)::int + (l.topic2 IS NOT NULL)::int + (l.topic3 IS NOT NULL)::int AS indexed\n\
         FROM {{{{logs({})}}}} l\nWHERE l.topic0 = {}{address}\nLIMIT 1",
        event.macro_args.join(", "),
        sig::bytea(event.topic0.as_slice()),
    );
    let probe = rewrite::rewrite(&probe, &mut NoAbis).ok()?.sql;
    let (rows, _) = execute(session, &probe, opts, 1, 0, opts.timeout.min(30)).ok()?;
    let name = event.signature.split('(').next().unwrap_or("");
    let place = event.address.as_ref().map(|a| format!(" from {a}")).unwrap_or_default();
    match rows.first().and_then(|r| r["indexed"].as_f64()).map(|n| n as usize) {
        None => Some(format!(
            "no `{name}` log{place} in this window: the event did not fire there, or its argument types differ \
             (topic0 is the hash of `{name}(types)`)"
        )),
        Some(found) if found != event.indexed => Some(format!(
            "`{name}` logs{place} carry {found} indexed argument(s), but the signature declares {}: fix the `indexed` flags \
             (`dedaub-monitoring sig {name}` lists the known layouts)",
            event.indexed
        )),
        Some(_) => Some(format!("`{name}` did fire{place} in this window: the other conditions of the query remove its rows")),
    }
}

struct NoAbis;

impl Abis for NoAbis {
    fn events(&mut self, _: &str) -> Res<Vec<EventAbi>> {
        Ok(Vec::new())
    }
}

pub fn summary(outcome: &Outcome, opts: &QueryOpts) -> String {
    let rows = outcome.table.rows.len();
    let mut text = format!("# {rows} row{} in {:.1} s", if rows == 1 { "" } else { "s" }, outcome.seconds);
    if outcome.queued >= 2.0 {
        text.push_str(&format!(", of which {:.1} s waiting for a query slot", outcome.queued));
    }
    if rows as u32 == opts.limit {
        text.push_str(&format!(
            " (the limit: there may be more; --limit goes up to {MAX_LIMIT}, --offset {} gives the next page)",
            opts.offset + opts.limit
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.to_string(), ty: ty.to_string() }
    }

    #[test]
    fn numeric_columns_are_selected_as_text() {
        let wrapped = exact_numerics("SELECT 1 -- x", &[col("tx", "bytea"), col("gas", "numeric"), col("to", "bytea")]).unwrap();
        assert_eq!(wrapped, "SELECT _q.\"tx\", _q.\"gas\"::text AS \"gas\", _q.\"to\"\nFROM (\nSELECT 1 -- x\n) _q");
    }

    #[test]
    fn queries_without_numerics_or_with_clashing_names_go_as_written() {
        assert_eq!(exact_numerics("SELECT 1", &[col("n", "int8")]), None);
        assert_eq!(exact_numerics("SELECT 1", &[col("gas", "numeric"), col("gas", "numeric")]), None);
    }
}
