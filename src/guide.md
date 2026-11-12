# dedaub-monitoring: query guide

The on-chain history of EVM chains in PostgreSQL: every call frame, event and transaction, token balances, transfers and allowances, and every contract with its source or decompiled code.
`--chain` picks the chain (a name or an id, default `ethereum`), so no table, macro or function takes a chain prefix.

This page: the lookups, `sql` with its tables and helpers, and the rules that keep a query fast. Three more pages: `guide contracts` (code, proxies), `guide tokens` (balances, transfers, allowances, USD), `guide incidents` (who lost what, who else runs the code, where the money went).

## Start with a lookup

A lookup writes its SQL in a shape that returns in seconds, and decodes the rows.

| Question | Command |
|---|---|
| Who calls this function, with how much gas, and does it fail? | `calls 0xCONTRACT [FUNCTION]` |
| How often is each function called, by whom, and how does it end? | `calls 0xCONTRACT --count-by function` |
| What does this contract call, and does it go on when a call fails? | `subcalls 0xCONTRACT` |
| What did it emit? | `events 0xCONTRACT [EVENT]` |
| What happened inside this transaction? | `tx 0xHASH` (`--args` decodes the arguments) |
| Does this transaction still work under the next fork's gas rules? | `replay 0xHASH` |
| Which storage slots did it create, clear or change? | `state 0xHASH` |
| What is this name, selector or topic? What are a contract's functions? | `sig NAME`, `sig 0xa9059cbb`, `abi 0xCONTRACT` |
| What is this contract's code, verified or not? | `code 0xCONTRACT` |
| Which contracts have these functions, this deployer, this code, this name? | `contracts 'f(uint256)' …`, `contracts --name TEXT` |
| Which tables and columns exist? | `schema`, `schema logs` |

- `FUNCTION` is a name from the contract's ABI, a signature, or a selector. `EVENT` is a name, a signature, or a topic0.
- `calls` and `subcalls` take `--failed`, `--returns` (outputs, or the revert reason of a failed call) and `--tx` (sender, gas limit, gas used and status of the call's transaction). `calls` also takes `--top-level` and `--from 0xCALLER`; `subcalls` takes `--to 0xCALLEE` and `--tolerated` (the call failed and the function that made it went on).
- `--count-by KEY` counts the calls instead of listing them: per `function`, `caller` (`callee` for `subcalls`) or `status`, or several (`function,status`), with the failures, the least and most gas given, the most gas used, and the first and last time.
- The filters and the counts look at the newest 20,000 calls of the window (`--scan N` for more); a note says how far back that reached.
- `calls`, `subcalls` and `events` take `--last 30d`, `--limit 20`, `--offset N` and `--max-cell N` (0 = no cut). Every lookup takes `--format tsv|json|csv`, and `--show-sql`, which prints the SQL it ran: a starting point for your own.
- For more than fifty rows, write the result to a file and work on that: `calls 0xCONTRACT --limit 500 > calls.tsv` (notes and the row count stay on stderr).

## sql

```sh
dedaub-monitoring sql "SELECT …"        # or --file q.sql, or the SQL on stdin
```

Rows go to stdout (TSV with a header line; NULL prints as `null`), notes and the row count to stderr. Numbers are exact, bytes print as `0x…`, nothing is cut. `--limit` is 100 by default and at most 500; `--offset` pages. An account runs 3 queries at a time: further commands wait in line (`--wait`, 600 s), so do not wrap them in retry loops. A query that runs past `--timeout` (100 s) is revoked.

### Tables

Three tables hold the history. A macro reads one of them with a block bound: `{{transaction_detail(duration='2d')}}`; without `duration` it takes `--last` (24h). A bare table name has no bound: give it one (rule 1), or constants on its primary key.

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `transaction_detail` | call frame, internal calls included | `block_number`, `tx_index`, `vm_step_start` (the key; step 0 is the transaction's own call), `from_a` (caller), `to_a` (callee), `calldata`, `returndata`, `callvalue`, `gas` (given to the frame; at step 0 the transaction's gas limit), `gas_used`, `error` (NULL, `ErrOutOfGas`, `ErrExecutionReverted`, …), `committed` (false: undone by a revert, its own or one above it), `call_opcode` (`CALL`, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`), `caller_vm_step_stack` (the steps of the frames above, nearest first; its `cardinality` is the depth) | `(to_a, selector(calldata), block_number)`, `(to_a, block_number)`, `(from_a, block_number)`, `(selector(calldata), block_number)`, the key |
| `logs` | event | `block_number`, `tx_index`, `log_index`, `address` (emitter), `topic0`…`topic3`, `data`, `committed` | `(address, block_number)`, `(topic0, block_number)` |
| `outer_transaction` | transaction | `block_number`, `tx_index` (the key), `tx_hash`, `from_a`, `to_a`, `status`, `gas` (limit), `gas_used`, `gas_price`, `callvalue`, `nonce`, `tx_type`. No calldata: a transaction's input is the `calldata` of its frame at step 0 | the key, `(tx_hash)`, `(from_a)`; none on `to_a` |

The macros return committed rows only. A failed call is not committed: write `only_committed=False` to see it. A bare table returns both kinds. Another table: `block` (`block_number`, `ts`, `gas_limit`, `base_fee_per_gas`). `schema NAME` shows any table.

### Literals and helpers

- An unquoted `0x…` of 20, 32, 16 or 4 bytes (an address, a hash, a code id, a selector) is a bytes literal: `WHERE d.to_a = 0xdAC17F958D2ee523a2206206994597C13D831ec7`. In quotes it is text and matches nothing.
- `selector('transfer(address,uint256)')`, `topic0('Transfer(address,address,uint256)')` and `keccak256('any text')` are the hash of what they are given; `selector(d.calldata)` is the selector of a call.
- `tx_hash(block_number, tx_index)` is the transaction hash and `block_timestamp(block_number)` the block's time (`ts(block_number)` is the same function). Do not join `outer_transaction` or `block` for them. Call them in the outermost SELECT, on the rows that are printed.
- `get_historical_block_number('2 day')` is the first block of the last two days.

## The rules that keep a query fast

A query costs what it reads. The history tables are stored in chunks by block number. Times are from Ethereum.

**1. Bound the blocks of every table in the query.** `duration='2d'` in a macro is `block_number > get_historical_block_number('2 day')` on the bare table. The bound keeps the query out of the older chunks, and the database works it out once, when it plans the query.

**2. The newest rows of one contract are cheap over any window.** An index leads to them, `ORDER BY block_number DESC` follows the index, and `LIMIT` stops the scan: 1 to 3 s over 30 days or a year, on the busiest contract. The lookups are this query.

**3. A filter, sort or aggregate on a column outside the index reads every row of the window, so make the window short.** That is any condition on `error`, `gas`, `gas_used`, `vm_step_start` or a decoded argument, any `GROUP BY`, and any `ORDER BY` other than `block_number`. Start with two days: 2 to 7 s for `handleOps` on the EntryPoint (47,000 calls), 7 to 30 s for USDT or WETH (over a million calls); over 30 days all three take a minute or time out. The first read of a range comes from disk and can take ten times as long as the second. For a count over a longer window use `calls --count-by`, which reads the newest 20,000 calls.

**4. Do not join the big tables to each other, and never join into `transaction_detail`.** For the transaction of each row (sender, gas limit, status), collect the rows first in a materialized CTE ordered by `(block_number, tx_index)`, then join `outer_transaction` on those two columns under the same block bound. A small set is then read by key, and a large one is merged in order: 27 events take 2 s, and 1.6 million take 70 s.

```sql
WITH a AS MATERIALIZED (
    SELECT l.block_number, l.tx_index, l.nonce, l.success
    FROM {{logs('0x27ca963c279c93801941e1eb8799c23f407d68e7.InboundMessageDispatched(uint64 indexed nonce, bytes32 topic, bool success, bytes32 rewardAddress)', duration='2d')}} l
    ORDER BY l.block_number, l.tx_index
)
SELECT o.tx_hash, o.from_a AS sender, o.gas AS gas_limit, o.gas_used, o.status, a.nonce, a.success
FROM outer_transaction o
JOIN a USING (block_number, tx_index)
WHERE o.block_number > get_historical_block_number('2 day')
```

For the frame that made a call (`caller_vm_step_stack[1]` is its step), limit your rows first, then read that frame with a scalar subquery on the key `(block_number, tx_index, vm_step_start)`: `subcalls 0xCONTRACT --show-sql` prints one.

**5. Transactions sent to a contract are its frames at `vm_step_start = 0`.** `outer_transaction` has no index on `to_a`: a condition on it reads every transaction of the window, 16 s over two days and a timeout over 30. The frames have the index, and `calls --top-level` reads them.

**6. One transaction is read by its position.** `tx 0xHASH` does it. In SQL, the hash has its own index in `outer_transaction`: `WHERE (d.block_number, d.tx_index) = (SELECT o.block_number, o.tx_index FROM outer_transaction o WHERE o.tx_hash = 0x…)`. That takes 2 s for a transaction's rows in `token_ledger` and 7 to 30 s for its frames, as the database learns the block only while it runs; with the two numbers written out as constants the frames take 1 to 3 s.

**7. Rows older than about three months are stored by contract.** Older chunks are compressed: grouped by `to_a` (frames) or by `address` (logs, `token_ledger`) and ordered by block. So `calls 0xCONTRACT --last 365d` still takes seconds, while a read by block or transaction alone scans whole chunks and times out. `tx` then reads the platform's trace, which lacks the gas each frame was given. The ledger's plain rows go back five months or more; `outer_transaction` is not compressed.

**8. Give the planner a small set as an array**: `WHERE c.md5_bytecode = ANY (ARRAY(SELECT …))`. The planner expects thousands of contracts for one bytecode and of proxies for one implementation, so for a join it may scan a table of 60 million rows. The second query of `guide incidents` went from a timeout to 3 s that way.

### When a query fails

- Exit codes: 0 rows printed (possibly none), 1 the server refused or failed the SQL, 2 bad arguments, 3 not logged in, 4 the query slots stayed busy for `--wait`, 5 revoked after `--timeout`.
- After a timeout, shorten the window (rule 3) and check the joins (rule 4) before raising `--timeout`. `explain "SQL"` prints the plan without running the query: dozens of `_hyper_…_chunk` scans mean a table without a block bound. `expand "SQL"` prints the SQL as the database receives it.
- A column that does not exist is answered with the columns the table has. Result columns that share a name are renamed (`max`, `max_2`).
- `status` checks the login and shows how many query slots are in use.

## Reading arguments with a signature

- `{{transaction_detail('0xCONTRACT.fn(type name, …)', outputs='(type name)', duration='2d')}}` selects the calls of one function. Every named argument and output is a column, in lower case; a struct argument `(…) msg` gives one column per field, `msg_field`. Without `0xCONTRACT.` it matches the selector on every contract.
- `{{logs('0xCONTRACT.Event(type indexed name, type name)')}}` selects one event, with its named arguments as columns (rule 4 has an example).
- The `indexed` flags decide which topics the macro matches. Write none at all, and this tool fills in the flags and the names from the known ABI. Flags you write are sent as written; if they are wrong the query returns no rows, and a note says how many indexed arguments the logs really carry.
- A signature makes the macro decode every row it reads. When you do not use the arguments, select with `to_a` and `selector(calldata)` instead.
