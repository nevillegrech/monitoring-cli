//! Function and event signatures: parsing, selectors and topics, ABI records of the API, and
//! decoding of calldata and logs.

use alloy_dyn_abi::{DynSolValue, EventExt, FunctionExt, JsonAbiExt};
use alloy_json_abi::{Event, Function, Param};
use alloy_primitives::{B256, hex};
use serde_json::{Value, json};

use crate::error::{Fail, Res};

pub fn parse_function(signature: &str) -> Res<Function> {
    Function::parse(signature.trim())
        .map_err(|e| Fail::Usage(format!("`{signature}` is not a function signature such as `transfer(address,uint256)`: {e}")))
}

pub fn parse_event(signature: &str) -> Res<Event> {
    Event::parse(signature.trim()).map_err(|e| {
        Fail::Usage(format!(
            "`{signature}` is not an event signature such as `Transfer(address indexed from, address indexed to, uint256 value)`: {e}"
        ))
    })
}

pub fn bytea(bytes: &[u8]) -> String {
    format!("'\\x{}'::bytea", hex::encode(bytes))
}

/// An event with every argument layout seen on chain: the same types can be declared with
/// different `indexed` flags, which share a topic0 but not a decoding.
#[derive(Clone, Debug)]
pub struct EventAbi {
    pub event: Event,
    pub layouts: Vec<Vec<bool>>,
}

impl EventAbi {
    /// One item of `/api/search/event`.
    pub fn from_api(item: &Value) -> Option<Self> {
        let name = item_name(item)?;
        let event: Event =
            serde_json::from_value(json!({"type": "event", "name": name, "inputs": item["inputs"], "anonymous": false})).ok()?;
        let declared: Vec<bool> = event.inputs.iter().map(|p| p.indexed).collect();
        let mut layouts: Vec<Vec<bool>> = item["indexes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_array().map(|flags| flags.iter().map(|f| f.as_bool().unwrap_or(false)).collect::<Vec<_>>()))
            .filter(|l: &Vec<bool>| l.len() == declared.len())
            .collect();
        if !layouts.contains(&declared) {
            layouts.insert(0, declared);
        }
        Some(Self { event, layouts })
    }

    pub fn with_layout(&self, layout: &[bool]) -> Event {
        let mut event = self.event.clone();
        for (param, indexed) in event.inputs.iter_mut().zip(layout) {
            param.indexed = *indexed;
        }
        event
    }

    /// The layouts with `topics` indexed arguments, the declared one first.
    pub fn candidates(&self, topics: usize) -> Vec<Event> {
        let declared: Vec<bool> = self.event.inputs.iter().map(|p| p.indexed).collect();
        let mut layouts: Vec<&Vec<bool>> = self.layouts.iter().filter(|l| l.iter().filter(|x| **x).count() == topics).collect();
        layouts.sort_by_key(|l| **l != declared);
        layouts.into_iter().map(|l| self.with_layout(l)).collect()
    }
}

fn item_name(item: &Value) -> Option<String> {
    item["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or_else(|| item["signature"].as_str().and_then(|s| s.split('(').next()))
        .map(str::to_string)
}

/// A function record of `/api/search/function`, or the `abi` object of a contract's ABI.
pub fn function_from_api(item: &Value) -> Option<Function> {
    let name = item_name(item)?;
    let mutability = item["stateMutability"].as_str().unwrap_or("nonpayable");
    serde_json::from_value(json!({
        "type": "function",
        "name": name,
        "inputs": item["inputs"],
        "outputs": item.get("outputs").filter(|o| o.is_array()).cloned().unwrap_or_else(|| json!([])),
        "stateMutability": mutability,
    }))
    .ok()
}

fn params_text(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| if p.name.is_empty() { p.selector_type().into_owned() } else { format!("{} {}", p.selector_type(), p.name) })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `name(type name, …)`, and ` returns (…)` when the function has outputs.
pub fn function_text(function: &Function) -> String {
    let mut text = format!("{}({})", function.name, params_text(&function.inputs));
    if !function.outputs.is_empty() {
        text.push_str(&format!(" returns ({})", params_text(&function.outputs)));
    }
    text
}

/// `Name(type indexed name, …)`: the form the `logs` macro takes.
pub fn event_text(event: &Event) -> String {
    let params: Vec<String> = event
        .inputs
        .iter()
        .map(|p| {
            let mut text = p.selector_type().into_owned();
            if p.indexed {
                text.push_str(" indexed");
            }
            if !p.name.is_empty() {
                text.push(' ');
                text.push_str(&p.name);
            }
            text
        })
        .collect();
    format!("{}({})", event.name, params.join(", "))
}

pub fn show(value: &DynSolValue) -> String {
    match value {
        DynSolValue::Address(a) => format!("{a:#x}"),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::FixedBytes(word, size) => format!("0x{}", hex::encode(&word[..*size])),
        DynSolValue::Bytes(bytes) => format!("0x{}", hex::encode(bytes)),
        DynSolValue::String(s) => serde_json::to_string(s).expect("string serializes"),
        DynSolValue::Function(f) => format!("0x{}", hex::encode(f.as_slice())),
        DynSolValue::Array(items) | DynSolValue::FixedArray(items) => {
            format!("[{}]", items.iter().map(show).collect::<Vec<_>>().join(", "))
        }
        DynSolValue::Tuple(items) => format!("({})", items.iter().map(show).collect::<Vec<_>>().join(", ")),
    }
}

fn named(names: impl Iterator<Item = String>, values: &[DynSolValue]) -> String {
    names
        .zip(values)
        .enumerate()
        .map(|(i, (name, value))| {
            let name = if name.is_empty() { format!("arg{i}") } else { name };
            format!("{name}={}", show(value))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `name=value, …` for the arguments in `calldata` (selector included).
pub fn decode_call(function: &Function, calldata: &[u8]) -> Option<String> {
    let values = function.abi_decode_input(calldata.get(4..)?).ok()?;
    Some(named(function.inputs.iter().map(|p| p.name.clone()), &values))
}

/// The arguments in `data` (selector included) as `value, …`, for an error's `Name(…)`.
pub fn decode_values(function: &Function, data: &[u8]) -> Option<String> {
    let values = function.abi_decode_input(data.get(4..)?).ok()?;
    Some(values.iter().map(show).collect::<Vec<_>>().join(", "))
}

pub fn decode_return(function: &Function, returndata: &[u8]) -> Option<String> {
    if function.outputs.is_empty() || returndata.is_empty() {
        return None;
    }
    let values = function.abi_decode_output(returndata).ok()?;
    Some(named(function.outputs.iter().map(|p| p.name.clone()), &values))
}

/// `name=value, …` in declaration order, from all the log's topics (topic0 first) and its data.
pub fn decode_log(event: &Event, topics: &[B256], data: &[u8]) -> Option<String> {
    let decoded = event.decode_log_parts(topics.iter().copied(), data).ok()?;
    let (mut indexed, mut body) = (decoded.indexed.iter(), decoded.body.iter());
    let values: Vec<DynSolValue> =
        event.inputs.iter().map(|p| if p.indexed { indexed.next() } else { body.next() }.cloned()).collect::<Option<_>>()?;
    Some(named(event.inputs.iter().map(|p| p.name.clone()), &values))
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    hex::decode(text.trim_start_matches("0x").trim_start_matches("\\x")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_and_topics() {
        assert_eq!(hex::encode(parse_function("transfer(address to, uint256 value)").unwrap().selector()), "a9059cbb");
        assert_eq!(
            hex::encode(parse_event("Transfer(address indexed from, address indexed to, uint256 value)").unwrap().selector()),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
        assert_eq!(bytea(&[0xa9, 0x05]), "'\\xa905'::bytea");
        assert!(parse_function("not a signature").is_err());
    }

    fn transfer_abi() -> EventAbi {
        EventAbi::from_api(&json!({
            "selector": "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
            "signature": "Transfer(address,address,uint256)",
            "name": "Transfer",
            "inputs": [
                {"name": "from", "type": "address", "indexed": true, "internalType": "address"},
                {"name": "to", "type": "address", "indexed": true, "internalType": "address"},
                {"name": "value", "type": "uint256", "indexed": false, "internalType": "uint256"}
            ],
            "type": "event",
            "indexes": [[true, true, false], [false, false, false], [true, true, true]]
        }))
        .unwrap()
    }

    #[test]
    fn reads_event_records_and_their_layouts() {
        let abi = transfer_abi();
        assert_eq!(event_text(&abi.event), "Transfer(address indexed from, address indexed to, uint256 value)");
        assert_eq!(abi.layouts.len(), 3);
        let erc721 = abi.candidates(3);
        assert_eq!(event_text(&erc721[0]), "Transfer(address indexed from, address indexed to, uint256 indexed value)");
        assert!(abi.candidates(1).is_empty());
    }

    #[test]
    fn decodes_a_log_in_declaration_order() {
        let abi = transfer_abi();
        let word = |tail: &str| B256::from_slice(&hex::decode(format!("{tail:0>64}")).unwrap());
        let topics = [abi.event.selector(), word("aa"), word("bb")];
        let data = hex::decode(format!("{:0>64}", "64")).unwrap();
        assert_eq!(
            decode_log(&abi.event, &topics, &data).unwrap(),
            "from=0x00000000000000000000000000000000000000aa, to=0x00000000000000000000000000000000000000bb, value=100"
        );
        assert_eq!(decode_log(&abi.event, &topics[..2], &data), None);
    }

    #[test]
    fn decodes_calls_and_tuples() {
        let function = function_from_api(&json!({
            "name": "execute",
            "inputs": [
                {"name": "call", "type": "tuple", "components": [
                    {"name": "target", "type": "address"}, {"name": "value", "type": "uint256"}, {"name": "data", "type": "bytes"}]},
                {"name": "", "type": "uint256"}
            ],
            "outputs": [{"name": "ok", "type": "bool"}],
            "stateMutability": "payable"
        }))
        .unwrap();
        assert_eq!(function_text(&function), "execute((address,uint256,bytes) call, uint256) returns (bool ok)");
        let calldata = hex::decode(concat!(
            "ba6dbe69",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000007",
            "00000000000000000000000000000000000000000000000000000000000000cc",
            "0000000000000000000000000000000000000000000000000000000000000005",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000002",
            "abcd000000000000000000000000000000000000000000000000000000000000"
        ))
        .unwrap();
        assert_eq!(hex::encode(function.selector()), "ba6dbe69");
        assert_eq!(decode_call(&function, &calldata).unwrap(), "call=(0x00000000000000000000000000000000000000cc, 5, 0xabcd), arg1=7");
        let returned = hex::decode(format!("{:0>64}", "1")).unwrap();
        assert_eq!(decode_return(&function, &returned).unwrap(), "ok=true");
    }
}
