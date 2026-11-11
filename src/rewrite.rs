//! Changes made to the SQL before it is sent. `--show-sql` prints the result.
//!
//! - a bare `0x…` of 4, 16, 20 or 32 bytes (a selector, a code id, an address, a hash) becomes
//!   a `bytea` literal (the server does this for addresses only);
//! - `selector('transfer(address,uint256)')` and `topic0('Transfer(address,address,uint256)')`
//!   become the hash they stand for;
//! - an event signature given to the `logs` macro without any `indexed` flag gets the flags and
//!   argument names of the known ABI: the macro matches topics by these flags, and a wrong
//!   layout returns no rows without an error;
//! - trailing semicolons go, which the server rejects.

use alloy_primitives::B256;

use crate::error::{Fail, Res};
use crate::sig::{self, EventAbi};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Code,
    /// A single-quoted string, quotes included.
    Str,
    /// A double-quoted identifier.
    Ident,
    Comment,
    /// `{{ … }}`, `{% … %}` or `{# … #}`.
    Jinja,
}

#[derive(Debug, Clone, PartialEq)]
struct Seg {
    kind: Kind,
    text: String,
}

/// Looks up the known ABIs of an event by its canonical signature, `Name(type,type)`.
pub trait Abis {
    fn events(&mut self, signature: &str) -> Res<Vec<EventAbi>>;
}

/// An event signature used in a `logs` macro, kept to explain an empty result.
#[derive(Debug, Clone, PartialEq)]
pub struct EventUse {
    pub address: Option<String>,
    pub signature: String,
    pub topic0: B256,
    pub indexed: usize,
    /// The macro's other arguments, as written (`duration='30d'`, `only_committed=False`).
    pub macro_args: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Rewritten {
    pub sql: String,
    pub notes: Vec<String>,
    pub events: Vec<EventUse>,
}

fn lex(sql: &str) -> Vec<Seg> {
    let bytes = sql.as_bytes();
    let mut segs: Vec<Seg> = Vec::new();
    let mut code_start = 0;
    let mut i = 0;
    let push = |segs: &mut Vec<Seg>, kind: Kind, from: usize, to: usize| {
        if from < to {
            segs.push(Seg { kind, text: sql[from..to].to_string() });
        }
    };
    while i < bytes.len() {
        let rest = &bytes[i..];
        let end = if rest.starts_with(b"--") {
            Some((Kind::Comment, i + rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len())))
        } else if rest.starts_with(b"/*") {
            Some((Kind::Comment, block_comment_end(bytes, i)))
        } else if rest.starts_with(b"{{") {
            Some((Kind::Jinja, jinja_end(bytes, i, b"}}")))
        } else if rest.starts_with(b"{%") {
            Some((Kind::Jinja, jinja_end(bytes, i, b"%}")))
        } else if rest.starts_with(b"{#") {
            Some((Kind::Jinja, jinja_end(bytes, i, b"#}")))
        } else if bytes[i] == b'\'' {
            let escapes = i > 0 && matches!(bytes[i - 1], b'E' | b'e') && (i < 2 || !is_word(bytes[i - 2]));
            Some((Kind::Str, quoted_end(bytes, i, b'\'', escapes)))
        } else if bytes[i] == b'"' {
            Some((Kind::Ident, quoted_end(bytes, i, b'"', false)))
        } else {
            None
        };
        match end {
            Some((kind, end)) => {
                push(&mut segs, Kind::Code, code_start, i);
                push(&mut segs, kind, i, end);
                i = end;
                code_start = end;
            }
            None => i += 1,
        }
    }
    push(&mut segs, Kind::Code, code_start, bytes.len());
    segs
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn quoted_end(bytes: &[u8], start: usize, quote: u8, backslash_escapes: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if backslash_escapes && bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn jinja_end(bytes: &[u8], start: usize, close: &[u8]) -> usize {
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            i = quoted_end(bytes, i, bytes[i], true);
        } else if bytes[i..].starts_with(close) {
            return i + close.len();
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// The inside of a quoted segment.
fn unquote(text: &str) -> &str {
    if text.len() >= 2 { &text[1..text.len() - 1] } else { "" }
}

fn is_hex_of(text: &str, digits: &[usize]) -> bool {
    text.strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .is_some_and(|hex| digits.contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Bare `0x` literals of 4, 16, 20 or 32 bytes. Other lengths stay the numbers they are in SQL.
fn rewrite_hex(code: &str, prev_is_word: bool) -> String {
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < bytes.len() {
        let boundary = if i == 0 { !prev_is_word } else { !is_word(bytes[i - 1]) && bytes[i - 1] != b'.' };
        if boundary && bytes[i] == b'0' && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
            let digits = bytes[i + 2..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
            let end = i + 2 + digits;
            let closed = bytes.get(end).is_none_or(|b| !is_word(*b));
            if closed && matches!(digits, 8 | 32 | 40 | 64) {
                out.push_str(&format!("'\\x{}'::bytea", code[i + 2..end].to_ascii_lowercase()));
                i = end;
                continue;
            }
        }
        let ch = code[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `selector('f(uint256)')` and `topic0('E(address)')`, written as SQL function calls.
fn rewrite_hash_calls(segs: &mut Vec<Seg>) -> Res<()> {
    let mut i = 0;
    while i + 2 < segs.len() {
        let call = if segs[i].kind == Kind::Code && segs[i + 1].kind == Kind::Str && segs[i + 2].kind == Kind::Code {
            hash_call(&segs[i].text, &segs[i + 2].text)
        } else {
            None
        };
        // A hex literal is calldata for the server's own `selector(bytea)`.
        let signature = unquote(&segs[i + 1].text).to_string();
        if let Some((name, head_len, tail_skip)) = call.filter(|_| signature.contains('(')) {
            let literal = if name == "selector" {
                sig::bytea(sig::parse_function(&signature)?.selector().as_slice())
            } else {
                sig::bytea(sig::parse_event(&signature)?.selector().as_slice())
            };
            let merged = format!("{}{}{}", &segs[i].text[..head_len], literal, &segs[i + 2].text[tail_skip..]);
            segs.splice(i..i + 3, [Seg { kind: Kind::Code, text: merged }]);
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// If `before` ends in `selector(` or `topic0(` and `after` starts with `)`: the function's
/// name, the length of `before` without the call, and how much of `after` the `)` takes.
fn hash_call(before: &str, after: &str) -> Option<(&'static str, usize, usize)> {
    let head = before.trim_end().strip_suffix('(')?.trim_end();
    let name = ["selector", "topic0"].into_iter().find(|n| head.to_ascii_lowercase().ends_with(n))?;
    let start = head.len() - name.len();
    if head[..start].bytes().last().is_some_and(|b| is_word(b) || b == b'.') {
        return None;
    }
    let close = after.len() - after.trim_start().len();
    after[close..].starts_with(')').then_some((name, start, close + 1))
}

/// Split the arguments of a macro call at top-level commas.
fn split_args(args: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let (mut parts, mut depth, mut start, mut i) = (Vec::new(), 0i32, 0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = quoted_end(bytes, i, bytes[i], true);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(args[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if !args[start..].trim().is_empty() {
        parts.push(args[start..].trim().to_string());
    }
    parts
}

fn is_quoted(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0]
}

/// Complete the event signatures of the `logs(...)` calls in one Jinja block.
fn rewrite_logs_macros(jinja: &str, abis: &mut dyn Abis, out: &mut Rewritten) -> Res<String> {
    let bytes = jinja.as_bytes();
    let mut result = String::with_capacity(jinja.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let end = quoted_end(bytes, i, bytes[i], true);
            result.push_str(&jinja[i..end]);
            i = end;
            continue;
        }
        let at_logs = jinja[i..].starts_with("logs") && (i == 0 || !is_word(bytes[i - 1]));
        let open = at_logs.then(|| i + 4 + jinja[i + 4..].len() - jinja[i + 4..].trim_start().len());
        let close = open.filter(|o| bytes.get(*o) == Some(&b'(')).and_then(|o| matching_paren(bytes, o).map(|c| (o, c)));
        let Some((open, close)) = close else {
            let ch = jinja[i..].chars().next().expect("in bounds");
            result.push(ch);
            i += ch.len_utf8();
            continue;
        };
        let mut args = split_args(&jinja[open + 1..close]);
        let position = args.iter().position(|a| is_quoted(a) || a.replace(' ', "").starts_with("inputs="));
        if let Some(p) = position {
            let (prefix, literal) = match args[p].split_once('=') {
                Some((name, value)) if !is_quoted(&args[p]) => (format!("{}=", name.trim()), value.trim().to_string()),
                _ => (String::new(), args[p].clone()),
            };
            if is_quoted(&literal) {
                let others: Vec<String> = args.iter().enumerate().filter(|(k, _)| *k != p).map(|(_, a)| a.clone()).collect();
                if let Some(completed) = complete_event(unquote(&literal), others, abis, out)? {
                    args[p] = format!("{prefix}'{completed}'");
                }
            }
        }
        result.push_str(&format!("logs({})", args.join(", ")));
        i = close + 1;
    }
    Ok(result)
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let (mut depth, mut i) = (0, open);
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                i = quoted_end(bytes, i, bytes[i], true);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The new text of an event signature literal, or `None` to leave it as written.
fn complete_event(literal: &str, macro_args: Vec<String>, abis: &mut dyn Abis, out: &mut Rewritten) -> Res<Option<String>> {
    let (address, text) = match literal.split_once('.') {
        Some((addr, rest)) if is_hex_of(addr.trim(), &[40]) => (Some(addr.trim().to_ascii_lowercase()), rest),
        _ => (None, literal),
    };
    let Ok(written) = alloy_json_abi::Event::parse(text.trim()) else { return Ok(None) };
    let mut chosen = written.clone();
    let mut changed = false;
    if !written.inputs.is_empty() && written.inputs.iter().all(|p| !p.indexed) {
        let canonical = written.signature();
        let known: Vec<EventAbi> = abis.events(&canonical)?.into_iter().filter(|a| a.event.signature() == canonical).collect();
        match known.first() {
            None => out.notes.push(format!(
                "no ABI is known for `{canonical}`: the signature was sent as written, with no indexed argument. \
                 If the event declares any, add `indexed` to them, or the query returns no rows."
            )),
            Some(abi) => {
                chosen = abi.event.clone();
                for (param, own) in chosen.inputs.iter_mut().zip(&written.inputs) {
                    if !own.name.is_empty() {
                        param.name = own.name.clone();
                    }
                }
                changed = sig::event_text(&chosen) != sig::event_text(&written);
                if changed {
                    out.notes.push(format!("event signature completed from the known ABI: {}", sig::event_text(&chosen)));
                }
                let others: Vec<String> = abi
                    .layouts
                    .iter()
                    .map(|l| abi.with_layout(l))
                    .filter(|e| sig::event_text(e) != sig::event_text(&abi.event))
                    .map(|e| sig::event_text(&e))
                    .collect();
                if !others.is_empty() {
                    out.notes.push(format!(
                        "`{canonical}` is also emitted with other indexed layouts; to read those, write the flags yourself: {}",
                        others.join(" | ")
                    ));
                }
            }
        }
    }
    out.events.push(EventUse {
        address: address.clone(),
        signature: sig::event_text(&chosen),
        topic0: chosen.selector(),
        indexed: chosen.inputs.iter().filter(|p| p.indexed).count(),
        macro_args,
    });
    Ok(changed.then(|| match &address {
        Some(address) => format!("{address}.{}", sig::event_text(&chosen)),
        None => sig::event_text(&chosen),
    }))
}

/// A quoted `'0x…'` compared with a column is text, and matches nothing.
fn quoted_hex_notes(segs: &[Seg], notes: &mut Vec<String>) {
    for (i, seg) in segs.iter().enumerate() {
        if seg.kind != Kind::Str || !is_hex_of(unquote(&seg.text), &[8, 32, 40, 64]) || i == 0 {
            continue;
        }
        let before = segs[i - 1].text.trim_end();
        let compared = segs[i - 1].kind == Kind::Code
            && (before.ends_with('=') || before.ends_with("<>") || before.ends_with('(') || before.ends_with(','));
        // `inputs->>'to' = '0x…'` compares text with text, as intended.
        let json_text = i >= 3 && segs[i - 2].kind == Kind::Str && segs[i - 3].text.trim_end().ends_with("->>");
        if compared && !json_text {
            let shown = unquote(&seg.text);
            notes.push(format!(
                "'{}…' in quotes is text, not bytes, and matches no address, selector or hash: write it without quotes",
                &shown[..shown.len().min(12)]
            ));
        }
    }
}

fn strip_trailing_semicolons(segs: &mut [Seg]) {
    for seg in segs.iter_mut().rev() {
        match seg.kind {
            Kind::Comment => continue,
            Kind::Code if seg.text.trim().is_empty() => continue,
            Kind::Code => {
                let kept = seg.text.trim_end().trim_end_matches(|c: char| c == ';' || c.is_whitespace());
                let tail = &seg.text[seg.text.trim_end().len()..];
                seg.text = format!("{kept}{tail}");
                return;
            }
            _ => return,
        }
    }
}

pub fn rewrite(sql: &str, abis: &mut dyn Abis) -> Res<Rewritten> {
    let mut out = Rewritten::default();
    let mut segs = lex(sql);
    if !segs.iter().any(|s| s.kind != Kind::Comment && !s.text.trim().is_empty()) {
        return Err(Fail::Usage("no SQL given: pass it as an argument, with --file, or on stdin".to_string()));
    }
    quoted_hex_notes(&segs, &mut out.notes);
    rewrite_hash_calls(&mut segs)?;
    for i in 0..segs.len() {
        match segs[i].kind {
            Kind::Code => {
                let prev_is_word = i > 0 && segs[i - 1].text.bytes().last().is_some_and(is_word);
                segs[i].text = rewrite_hex(&segs[i].text, prev_is_word);
                if segs[i].text.contains("<chain>") {
                    out.notes
                        .push("`<chain>` is a placeholder: drop it (the chain comes from --chain) or write the chain's name".to_string());
                }
            }
            Kind::Jinja => segs[i].text = rewrite_logs_macros(&segs[i].text.clone(), abis, &mut out)?,
            _ => {}
        }
    }
    strip_trailing_semicolons(&mut segs);
    out.sql = segs.iter().map(|s| s.text.as_str()).collect::<String>().trim().to_string();
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use serde_json::json;

    use super::*;

    /// A fixed set of ABIs in place of the search API.
    pub struct Known(pub Vec<EventAbi>);

    impl Abis for Known {
        fn events(&mut self, signature: &str) -> Res<Vec<EventAbi>> {
            Ok(self.0.iter().filter(|a| a.event.signature() == signature).cloned().collect())
        }
    }

    fn known() -> Known {
        let record = |name: &str, inputs: serde_json::Value, indexes: serde_json::Value| {
            EventAbi::from_api(&json!({"name": name, "inputs": inputs, "indexes": indexes})).unwrap()
        };
        Known(vec![
            record(
                "WithdrawalFinalized",
                json!([{"name": "withdrawalHash", "type": "bytes32", "indexed": true}, {"name": "success", "type": "bool", "indexed": false}]),
                json!([[true, false]]),
            ),
            record(
                "Transfer",
                json!([{"name": "from", "type": "address", "indexed": true}, {"name": "to", "type": "address", "indexed": true}, {"name": "value", "type": "uint256", "indexed": false}]),
                json!([[true, true, false], [true, true, true]]),
            ),
        ])
    }

    fn run(sql: &str) -> Rewritten {
        rewrite(sql, &mut known()).unwrap()
    }

    #[test]
    fn bare_hex_of_4_16_20_and_32_bytes_becomes_bytea() {
        let sql = "SELECT 1 FROM t WHERE to_a = 0xdAC17F958D2ee523a2206206994597C13D831ec7 AND selector(calldata) = 0xa9059cbb \
                   AND topic0 = 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef AND gas > 0x10 AND n = 0x1234567 \
                   AND md5_bytecode = 0xB6D555D1194D3A5E0BFBBA73EAB269EC";
        assert_eq!(
            run(sql).sql,
            "SELECT 1 FROM t WHERE to_a = '\\xdac17f958d2ee523a2206206994597c13d831ec7'::bytea AND selector(calldata) = '\\xa9059cbb'::bytea \
             AND topic0 = '\\xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef'::bytea AND gas > 0x10 AND n = 0x1234567 \
             AND md5_bytecode = '\\xb6d555d1194d3a5e0bfbba73eab269ec'::bytea"
        );
    }

    #[test]
    fn strings_comments_identifiers_and_macros_are_left_alone() {
        let sql = "SELECT '0xa9059cbb' AS s, \"0xa9059cbb\" -- 0xa9059cbb\n/* 0xa9059cbb */ FROM {{transaction_detail('0xdac17f958d2ee523a2206206994597c13d831ec7.transfer(address,uint256)', duration='1h')}} d";
        assert_eq!(run(sql).sql, sql);
    }

    #[test]
    fn selector_and_topic0_of_a_signature() {
        assert_eq!(
            run("SELECT selector('transfer(address to, uint256 value)'), topic0( 'Transfer(address,address,uint256)' ) FROM t").sql,
            "SELECT '\\xa9059cbb'::bytea, '\\xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef'::bytea FROM t"
        );
        let untouched = "SELECT selector(d.calldata), common.selector('\\xa9059cbb'), reselector('f()') FROM t";
        assert_eq!(run(untouched).sql, untouched);
        assert!(matches!(rewrite("SELECT selector('f(uint256')", &mut known()), Err(Fail::Usage(_))));
    }

    #[test]
    fn an_event_without_flags_gets_the_known_layout_and_names() {
        let r = run("SELECT * FROM {{ logs('WithdrawalFinalized(bytes32, bool)', duration='30d') }} l LIMIT 5;");
        assert_eq!(
            r.sql,
            "SELECT * FROM {{ logs('WithdrawalFinalized(bytes32 indexed withdrawalHash, bool success)', duration='30d') }} l LIMIT 5"
        );
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].indexed, 1);
        assert_eq!(r.events[0].macro_args, vec!["duration='30d'".to_string()]);
        assert!(r.notes[0].contains("completed from the known ABI"));
    }

    #[test]
    fn the_address_prefix_the_inputs_keyword_and_own_names_survive() {
        let r = run(
            "SELECT * FROM {{ethereum.logs(duration='1h', inputs=\"0xDAC17F958D2ee523a2206206994597C13D831ec7.Transfer(address src, address dst, uint256 wad)\")}} l",
        );
        assert_eq!(
            r.sql,
            "SELECT * FROM {{ethereum.logs(duration='1h', inputs='0xdac17f958d2ee523a2206206994597c13d831ec7.Transfer(address indexed src, address indexed dst, uint256 wad)')}} l"
        );
        assert_eq!(r.events[0].address.as_deref(), Some("0xdac17f958d2ee523a2206206994597c13d831ec7"));
        assert!(r.notes.iter().any(|n| n.contains("other indexed layouts") && n.contains("uint256 indexed value")));
    }

    #[test]
    fn flags_written_by_hand_are_trusted() {
        let sql = "SELECT * FROM {{logs('Transfer(address indexed from, address indexed to, uint256 indexed id)', duration='1h')}} l";
        let r = run(sql);
        assert_eq!(r.sql, sql);
        assert_eq!(r.events[0].indexed, 3);
        assert!(r.notes.is_empty());
    }

    #[test]
    fn an_unknown_event_is_sent_as_written_with_a_note() {
        let sql = "SELECT * FROM {{logs('Mystery(uint256 a)', duration='1h')}} l";
        let r = run(sql);
        assert_eq!(r.sql, sql);
        assert!(r.notes[0].contains("no ABI is known"));
    }

    #[test]
    fn quoted_hex_compared_with_a_column_is_flagged() {
        let r = run(
            "SELECT 1 FROM t WHERE to_a = '0xdac17f958d2ee523a2206206994597c13d831ec7' AND inputs->>'to' = '0xdac17f958d2ee523a2206206994597c13d831ec7'",
        );
        assert_eq!(r.notes.len(), 1);
        assert!(r.notes[0].contains("is text, not bytes"));
    }

    #[test]
    fn trailing_semicolons_go_and_empty_sql_is_refused() {
        assert_eq!(run("SELECT 1;\n").sql, "SELECT 1");
        assert_eq!(run("SELECT ';' ; ; -- done").sql, "SELECT ';' -- done");
        assert!(matches!(rewrite("  -- nothing\n", &mut known()), Err(Fail::Usage(_))));
    }

    #[test]
    fn the_chain_placeholder_is_explained() {
        assert!(run("SELECT <chain>.ts(1)").notes[0].contains("placeholder"));
    }
}
