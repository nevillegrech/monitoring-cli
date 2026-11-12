# dedaub-monitoring

Query the on-chain history of Ethereum and other EVM chains from the command line: every call frame, event and transaction that the [Dedaub](https://app.dedaub.com) platform indexes, token balances, transfers and allowances with their USD values, and the code of every contract, as verified source or decompiled. The tool reads; it stores no queries and creates no alerts.

```console
$ dedaub-monitoring subcalls 0xd60247c6848b7ca29eddf63aa924e53db6ddd8ec --tolerated --tx --limit 2
tx       block     time                  callee    depth  op    gas   gas_used  value            status       function  args  caller_function  caller_status  tx_from   tx_gas_limit  tx_gas_used  tx_status
0xda2b…  26321549  2026-11-01T17:54:59Z  0x3d97…   3      CALL  2301  2301      860000000000000  ErrOutOfGas                  processMessage   ok             0x03ef…   732000        700245       ok
0x1e3f…  26321409  2026-11-01T17:26:59Z  0x4047…   3      CALL  2301  2301      452434158312595  ErrOutOfGas                  processMessage   ok             0x03ef…   732000        700221       ok
```

(Hashes and addresses are shortened here; the tool prints them whole.)

It is built for scripts and AI agents: tab-separated rows with exact numbers, nothing cut, one failure kind per exit code, and commands that wait in line for the account's query slots instead of failing.

## Install

You need a Rust toolchain, 1.89 or newer ([rustup.rs](https://rustup.rs)).

```sh
cargo install --locked --git https://github.com/Dedaub/monitoring-cli
dedaub-monitoring login          # prints a URL to open in a browser
dedaub-monitoring status         # exit 0: queries can run
```

The installers run the same steps and also install the agent skill:

```sh
curl -LsSf https://raw.githubusercontent.com/Dedaub/monitoring-cli/main/install.sh | sh
```

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/Dedaub/monitoring-cli/main/install.ps1 | iex"
```

From a clone, `make install-local` installs the working tree. The tool is developed and tested on Linux; the macOS and Windows builds are untested.

If the Python CLI (0.7 or earlier) is installed, remove it first with `uv tool uninstall monitoring-cli`. Its `dedaub-monitoring` sits in `~/.local/bin`, which often comes before cargo's `~/.cargo/bin` on `PATH`; `install.sh` and `make install` say so when that is the case.

Each use renews the login; left unused, it expires after 48 hours. On a machine without a browser, log in elsewhere, print the token with `dedaub-monitoring token`, and set it as `DEDAUB_MONITORING_REFRESH_TOKEN`. A token set this way works for 48 hours from the moment it was issued. Treat it as a password.

## Lookups

A lookup writes its own SQL, in a shape that returns in seconds, and decodes the rows with the ABI.

| Command | Shows |
|---|---|
| `calls 0xCONTRACT [FUNCTION]` | calls to a contract: caller, depth, gas given and used, status, arguments |
| `subcalls 0xCONTRACT` | calls a contract makes, and whether the calling function went on when one failed |
| `calls … --count-by function` | the calls counted per function, caller, callee or status, with the gas they were given and used |
| `events 0xCONTRACT [EVENT]` | its events, decoded |
| `tx 0xHASH` | the call frames of one transaction, of any age |
| `replay 0xHASH` | the transaction re-executed under today's gas rules and the next fork's |
| `state 0xHASH` | the storage slots, balances, nonces and code it changed |
| `abi 0xCONTRACT`, `sig NAME` | a contract's functions; a function or event by name, selector or topic0 |
| `code 0xCONTRACT` | a contract's verified source, or its decompiled code when it has none |
| `contracts [FUNCTION]…` | contracts that have these functions, verified or not; or by `--name`, `--deployer`, `--same-code` |
| `schema [TABLE]` | tables, columns and indexes |

```sh
dedaub-monitoring calls 0x7c7fdfca295a787ded12bb5c1a49a8d2cc20e3f8 transmit --tx --limit 5
dedaub-monitoring calls 0xdac17f958d2ee523a2206206994597c13d831ec7 transfer --failed --returns --last 7d
dedaub-monitoring calls 0x0000000071727de22e5e9d8baf0edac6f37da032 --count-by function,status --last 2d
dedaub-monitoring events 0x27ca963c279c93801941e1eb8799c23f407d68e7 InboundMessageDispatched --tx
dedaub-monitoring replay 0x9c06b464b5007c5c4e579fa9dac4cb3d78211ccbc88a22a93fa791361e28c6ce
dedaub-monitoring code 0x27ca963c279c93801941e1eb8799c23f407d68e7 --files
dedaub-monitoring contracts 'deposit(uint256,address)' 'redeem(uint256,address,address)' --no-source
```

`code` and `abi` work on contracts without verified source: `code` prints the decompiled code, and `abi` the functions found in the bytecode. Through a proxy, both show the implementation.

`--help` on a command lists its filters. `--show-sql` prints the SQL a lookup ran. `--chain` takes a chain name or id (`ethereum`, `1`, `base`, `8453`, …).

## SQL

```sh
dedaub-monitoring sql "
SELECT tx_hash(d.block_number, d.tx_index) AS tx, d.from_a AS caller, d.gas, d.gas_used, d.error
FROM {{transaction_detail(duration='30d', only_committed=False)}} d
WHERE d.to_a = 0xdAC17F958D2ee523a2206206994597C13D831ec7
  AND selector(d.calldata) = selector('transfer(address,uint256)')
ORDER BY d.block_number DESC
LIMIT 20"
```

The SQL also comes from `--file` or stdin. `dedaub-monitoring guide` prints the query guide: the tables and their indexes, the helpers, and the rules that keep a query fast: a block bound on every table, short windows for filters and aggregates, no joins into `transaction_detail`. Read it before writing SQL. It has three more pages: `guide contracts` (code and proxies), `guide tokens` (balances, transfers, allowances, USD) and `guide incidents` (who lost what, who else runs the code, where the money went).

Before the SQL is sent, the tool makes these changes (`--show-sql` prints the result, `--raw` turns them off):

- an unquoted `0x…` of 20, 32, 16 or 4 bytes (an address, a hash, a code id, a selector) becomes a bytes literal;
- `selector('f(uint256)')`, `topic0('E(address)')` and `keccak256('text')` become the hash they stand for;
- an event signature given to the `logs` macro without `indexed` flags gets the flags and argument names of the known ABI. If a query over an event returns nothing, a note says whether the flags are the reason;
- `numeric` columns are fetched as text, so that gas amounts print as integers and 256-bit values keep every digit;
- result columns that share a name are renamed (`max`, `max_2`): the API would return only one of them.

A column that does not exist is answered with the columns the table has.

Output is TSV by default, `--format json` gives one object per row, `--format csv` gives CSV. Rows go to stdout; notes and the row count go to stderr.

## Behaviour to rely on

- **Queueing.** An account runs three queries at a time. Commands on one machine share three slots through lock files, and wait up to `--wait` seconds (600) for a slot or for the server to accept the query. Do not wrap commands in retry loops.
- **Timeouts.** A query that runs past `--timeout` (100 s) is revoked on the server, as is a query interrupted with Ctrl-C. The default is under the 120 s after which an agent's shell gives up on a command, so the advice printed on a timeout is seen.
- **Exit codes.** 0 success, 1 the server refused or failed the query, 2 bad arguments, 3 not logged in, 4 the query slots stayed busy, 5 timeout, 130 interrupted.
- **State.** The login lives in `~/.config/dedaub/monitoring.json`. The access token, the id of the account's scratch query and the slot lock files live in `~/.cache/dedaub-monitoring` (`DEDAUB_MONITORING_STATE_DIR` moves it). In a sandbox that cannot write there, run `dedaub-monitoring status` once outside it: the sandboxed commands then find the lock files and the cached ids.
- **One stored query.** The API runs SQL only in the context of a stored query, so the tool keeps a single empty one per account, `/_cli/scratch`, and never writes to it.

## Agent skill

```sh
dedaub-monitoring install-skill            # Claude Code, and any of Codex, Cursor, .agents found here
```

The skill says when to use the tool, gives the five rules for writing SQL, and points to `dedaub-monitoring guide`. It is also published from this repository as a Claude Code plugin:

```
/plugin marketplace add Dedaub/monitoring-cli
/plugin install dedaub-monitoring@dedaub
```

## Changes from 0.7

Version 0.8 is a rewrite in Rust that only queries. The commands of the Python CLI that stored queries and managed alerts, dashboards, widgets and webhooks are gone, and so are `run-query`, `create-query` and `write-query`: `sql` replaces them and needs no query id. Use the web app for alerts, or the last Python release (commit `ec438f4`). A login made with the Python CLI keeps working: both read `~/.config/dedaub/monitoring.json`.

## Development

```sh
cargo test                                   # unit tests, no network
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT
