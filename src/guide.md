# dedaub-monitoring: query guide

The on-chain history of EVM chains in PostgreSQL: every call frame, event and transaction, token balances, transfers and allowances, and every contract with its source or decompiled code.
`--chain` picks the chain (a name or an id, default `ethereum`), so no table, macro or function takes a chain prefix.

## Start with a lookup

A lookup writes its SQL in a shape that returns in seconds, and decodes the rows.

| Question | Command |
|---|---|
| Who calls this function, with how much gas, and does it fail? | `calls 0xCONTRACT [FUNCTION]` |
| What does this contract call, and does it go on when a call fails? | `subcalls 0xCONTRACT` |
| What did it emit? | `events 0xCONTRACT [EVENT]` |
| What happened inside this transaction? | `tx 0xHASH` |
| Does this transaction still work under the next fork's gas rules? | `replay 0xHASH` |
| Which storage slots did it create, clear or change? | `state 0xHASH` |
| What is this name, selector or topic? What are a contract's functions? | `sig NAME`, `sig 0xa9059cbb`, `abi 0xCONTRACT` |
| What is this contract's code, verified or not? | `code 0xCONTRACT` |
| Which contracts have these functions, this deployer, this code, this name? | `contracts 'f(uint256)' …`, `contracts --name TEXT` |
| Which tables and columns exist? | `schema`, `schema logs` |

- `FUNCTION` is a name from the contract's ABI, a signature, or a selector. `EVENT` is a name, a signature, or a topic0.
- `code` prints the verified source: the files with code in the contract, the main one first (`--files` lists them all, `--file Gateway.sol` prints one, `--all` every one). A contract without verified source gets its decompiled code, which `--decompiled` asks for in any case. For a proxy it prints the implementation's code and says so; `--proxy` gives the proxy's own. `abi` on a contract without verified source lists the functions found in its bytecode, those of unknown signature by selector alone.
- `contracts` lists contracts newest first. Its arguments are functions the bytecode must have, as signatures or selectors: it finds verified and unverified contracts alike, in 6 to 15 s, or in 1 s when the functions are common. `--deployer 0xADDRESS` (the account that sent the creating transaction, or the factory), `--same-code 0xCONTRACT`, `--no-source` and `--last 30d` narrow it or stand alone. `--name TEXT` asks the platform's name search instead: its 30 best matches, with no other condition, under the platform's display name (a token shows its symbol). A proxy has only its own few functions, so a search by functions finds implementations.
- `calls` and `subcalls` take `--failed`, `--returns` (outputs, or the revert reason of a failed call) and `--tx` (sender, gas limit, gas used and status of the call's transaction). `calls` also takes `--top-level` and `--from 0xCALLER`; `subcalls` takes `--to 0xCALLEE` and `--tolerated` (the call failed and the function that made it went on). The filters (`--failed`, `--tolerated`, `--top-level`, `--from`, `--to`) test the newest 20,000 calls of the window (`--scan N` for more); when that leaves fewer rows than asked for, a note says how far back it reached.
- `tx --args` decodes every frame's arguments. `replay` runs the transaction three times: today's rules as mined, the next fork's rules at its own gas limit, and with the limit lifted; it prints status and gas per run and per frame.
- `calls`, `subcalls` and `events` take `--last 30d`, `--limit 20`, `--offset N` and `--max-cell N` (0 = no cut). Every lookup takes `--format tsv|json|csv`, and `--show-sql`, which prints the SQL it ran: a starting point for your own.

## sql

```sh
dedaub-monitoring sql "SELECT …"        # or --file q.sql, or the SQL on stdin
```

Rows go to stdout (TSV with a header line; NULL prints as `null`), notes and the row count to stderr. Numbers are exact, bytes print as `0x…`, nothing is cut. `--limit` is 100 by default and at most 500; `--offset` pages. An account runs 3 queries at a time: further commands wait in line (`--wait`, 600 s), so do not wrap them in retry loops. A query that runs past `--timeout` (120 s) is revoked.

### Tables

Three tables hold the history. A macro reads one of them with a block bound: `{{transaction_detail(duration='2d')}}`; without `duration` it takes `--last` (24h). A bare table name has no bound: give it one (rule 1 below), or constants on its primary key.

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `transaction_detail` | call frame, internal calls included | `block_number`, `tx_index`, `vm_step_start` (the key; step 0 is the transaction's own call), `from_a` (caller), `to_a` (callee), `calldata`, `returndata`, `callvalue`, `gas` (given to the frame; at step 0 the transaction's gas limit), `gas_used`, `error` (NULL, `ErrOutOfGas`, `ErrExecutionReverted`, …), `committed` (false: undone by a revert, its own or one above it), `call_opcode` (`CALL`, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`), `caller_vm_step_stack` (the steps of the frames above, nearest first; its `cardinality` is the depth) | `(to_a, selector(calldata), block_number)`, `(to_a, block_number)`, `(from_a, block_number)`, `(selector(calldata), block_number)`, the key |
| `logs` | event | `block_number`, `tx_index`, `log_index`, `address` (emitter), `topic0`…`topic3`, `data`, `committed` | `(address, block_number)`, `(topic0, block_number)` |
| `outer_transaction` | transaction | `block_number`, `tx_index` (the key), `tx_hash`, `from_a`, `to_a`, `status`, `gas` (limit), `gas_used`, `gas_price`, `callvalue`, `nonce`, `tx_type` | the key, `(tx_hash)`, `(from_a)`; none on `to_a` |

The macros return committed rows only. A failed call is not committed: write `only_committed=False` to see it. A bare table returns both kinds.

Another: `block` (`block_number`, `ts`, `gas_limit`, `base_fee_per_gas`). `schema NAME` shows any table.

### Contracts and their code

These tables are not split by block and need no block bound.

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `contracts` | contract | `address`, `md5_bytecode` (the code id: equal values are the same bytecode), `block_number` and `tx_index` (its creation), `deployer` (the address that created it: an account or a factory), `eoa_deployer` (the account that sent that transaction), `block_number_destroyed` | `(address)`, `(md5_bytecode)`, `(deployer)`, `(eoa_deployer)`, `(block_number)` |
| `decompiled_code` | bytecode, verified or not | `md5_bytecode`, `array_function_selectors`, `array_event_selectors` (topic0 values), `source_level` (the decompiled code) | `(md5_bytecode)`; none on the selectors |
| `code` | bytecode with verified source | `md5_bytecode`, `contract_name`, `compiler`, `json_abi`, `array_function_signatures` | `(md5_bytecode)` |
| `proxy_implementation_latest` | proxy | `proxy_address`, `implementation_address`, `proxy_type` | `(proxy_address)`, `(implementation_address)` |

- Search by functions in `decompiled_code`: it has the functions of every bytecode. On Ethereum it has 3 million rows and no index on the selectors, so a search reads them all: 6 to 15 s.
- Read `code` by `md5_bytecode` only, as a join or a subquery. A condition on its name or its selectors reads all 13 million rows and times out: the table has indexes for both, but the query role cannot use them. For a name, run `contracts --name`.
- Print a code as text with `code 0xCONTRACT`, not as a cell of a query.

```sql
SELECT c.address, c.block_number, k.contract_name,
       ARRAY(SELECT encode(s, 'hex') FROM unnest(d.array_function_selectors) s) AS selectors
FROM decompiled_code d
JOIN contracts c USING (md5_bytecode)
LEFT JOIN code k USING (md5_bytecode)
WHERE d.array_function_selectors @> ARRAY[selector('deposit(uint256,address)'), selector('redeem(uint256,address,address)')]::selector4[]
ORDER BY c.block_number DESC
LIMIT 20
```

### Tokens: balances, transfers, allowances

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `token_balance` | holder and token, as of now | `token_address`, `owner_address`, `value` (in the token's smallest unit), `block_number` (its last change) | `(token_address, owner_address)`, `(token_address, value)`, `(owner_address)` |
| `token_allowance` | owner, spender and token, as of now | `token_address`, `owner_address`, `spender_address`, `value` | `(token_address, spender_address, owner_address)`, `(owner_address)`, `(spender_address)` |
| `token_ledger` | side of a transfer | `block_number`, `tx_index`, `vm_step_start`, `vm_step`, `address`, `counterparty_address`, `token_address`, `value_delta`, `type` (`ERC20`, `NATIVE`, `SELF_DESTRUCT`) | `(address, block_number, tx_index, vm_step_start, vm_step)`; none on `token_address` |
| `network_token_info` | token | `token_address`, `symbol`, `decimals`, `last_price` (USD for one whole token), `last_cap`, `ts` (when the price was read) | `(token_address)` |

- The native token is `0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee` in all four tables.
- `token_ledger` has two rows for each transfer, one per side. On a row, a positive `value_delta` is what `address` sent to `counterparty_address`, and a negative one what it received. So what an account gained is **minus** the sum of `value_delta` over its rows. The view `token_transfers` is the positive rows, as `from_a`, `to_a` and `value`. The table is split by block like the three history tables: use the macro, `{{token_ledger(duration='2d')}}`, or a block bound.
- Transfers of an account: `token_ledger` by `address`, both directions in one index.
- Transfers of a token: its `Transfer` events in `logs`, where the token is the emitter and the index leads to them. The signature form decodes every event it reads: 5 s for an hour of USDT, 40 s for a day. The ledger has no index on the token; with a condition on `token_address` it reads every transfer of the window: 2 s for an hour and 5 to 15 s for a day, whatever the token. Native transfers are in the ledger only.
- USD: `to_usd_value(amount, token_address)` for a handful of rows. It is one lookup in `network_token_info` per call; it gives NULL for a token that is not there. For many rows join `network_token_info n USING (token_address)` and compute `value * n.last_price / 10 ^ n.decimals`.
- A USD figure is a price times a balance, and some balances are not money: a token with a supply of 4.5e39 puts one holder at 2e36 USD, at the top of any ranking. Read the token and the amount before trusting the figure. An ERC-721 transfer is a ledger row of type `ERC20` whose `value_delta` is the token id.
- The first read of the rows of a large holder, a busy account or a widely approved spender comes from disk and takes 10 s to over two minutes; the second run takes 2 to 30 s.

What an account holds, in USD:

```sql
SELECT b.token_address, n.symbol, b.value / 10 ^ n.decimals AS amount,
       b.value * n.last_price / 10 ^ n.decimals AS usd
FROM token_balance b
JOIN network_token_info n USING (token_address)
WHERE b.owner_address = 0xd8da6bf26964af9d7eed9e03e53415d37aa96045
  AND b.value > 0
ORDER BY usd DESC NULLS LAST
LIMIT 20
```

The latest transfers of an account, both directions, ordered as the index is:

```sql
SELECT tx_hash(l.block_number, l.tx_index) AS tx, ts(l.block_number) AS time,
       CASE WHEN l.value_delta > 0 THEN 'sent' ELSE 'received' END AS direction,
       l.counterparty_address, l.token_address, abs(l.value_delta) AS amount,
       to_usd_value(abs(l.value_delta), l.token_address) AS usd
FROM {{token_ledger(duration='30d')}} l
WHERE l.address = 0xd8da6bf26964af9d7eed9e03e53415d37aa96045
ORDER BY l.block_number DESC, l.tx_index DESC, l.vm_step_start DESC, l.vm_step DESC
LIMIT 20
```

The net flow of a busy account per token over two days:

```sql
SELECT l.token_address, n.symbol, count(*) AS transfers,
       -sum(l.value_delta) / 10 ^ n.decimals AS net_received,
       -sum(l.value_delta) * n.last_price / 10 ^ n.decimals AS net_usd
FROM {{token_ledger(duration='2d')}} l
JOIN network_token_info n USING (token_address)
WHERE l.address = 0x000000000004444c5dc75cb358380d2e3de08a90
GROUP BY l.token_address, n.symbol, n.decimals, n.last_price
ORDER BY abs(sum(l.value_delta) * n.last_price / 10 ^ n.decimals) DESC NULLS LAST
LIMIT 20
```

Who is exposed to a spender: the owners that approved it and still hold the token, with what it could take now. Join allowances to balances first, and the prices to the result.

```sql
SELECT x.owner_address, n.symbol, x.amount / 10 ^ n.decimals AS takeable,
       x.amount * n.last_price / 10 ^ n.decimals AS usd
FROM (
    SELECT token_address, owner_address, sum(least(a.value, b.value)) AS amount
    FROM token_allowance a
    JOIN token_balance b USING (token_address, owner_address)
    WHERE a.spender_address = 0x889edc2edab5f40e902b864ad4d7ade8e412f9b1
      AND a.value > 0
      AND b.value > 0
    GROUP BY token_address, owner_address
) x
JOIN network_token_info n USING (token_address)
ORDER BY usd DESC NULLS LAST
LIMIT 20
```

For the allowances an account gave, the condition is `a.owner_address = …`; for the largest holders of a token, `FROM token_balance b WHERE b.token_address = … ORDER BY b.value DESC`.

### Decoding with a signature

```sql
SELECT tx_hash(d.block_number, d.tx_index) AS tx, d.who, d.balance
FROM {{transaction_detail('0xdac17f958d2ee523a2206206994597c13d831ec7.balanceOf(address who)', outputs='(uint256 balance)', duration='10m')}} d
ORDER BY d.block_number DESC
LIMIT 5
```

- `transaction_detail('0xCONTRACT.fn(type name, …)', outputs='(type name)')` selects the calls of one function. Every named argument and output is a column, in lower case; a struct argument `(…) msg` gives one column per field, `msg_field`. Without `0xCONTRACT.` it matches the selector on every contract.
- `logs('0xCONTRACT.Event(type indexed name, type name)')` selects one event, with its named arguments as columns.
- The `indexed` flags decide which topics the macro matches. Write none at all, and this tool fills in the flags and the names from the known ABI and says so in a note. Flags you write are sent as written; if they are wrong the query returns no rows, and a note then says how many indexed arguments the logs really carry.
- A signature makes the macro decode every row it reads. Leave it out when you do not use the arguments, and select with `to_a` and `selector(calldata)` instead.

### Literals and helpers

- An unquoted `0x…` of 20, 32, 16 or 4 bytes (an address, a hash, a code id, a selector) is a bytes literal: `WHERE d.to_a = 0xdAC17F958D2ee523a2206206994597C13D831ec7`. In quotes it is text and matches nothing.
- `selector('transfer(address,uint256)')` and `topic0('Transfer(address,address,uint256)')` are the hash of a signature; `selector(d.calldata)` is the selector of a call.
- `tx_hash(block_number, tx_index)` is the transaction hash and `ts(block_number)` the block's time. Do not join `outer_transaction` or `block` for them.
- `get_historical_block_number('2 day')` is the first block of the last two days.

## Keeping a query fast

A query costs what it reads. The three tables are stored in chunks by block number, and a bound on `block_number` keeps a query out of the older chunks. Times are from Ethereum.

**1. Bound the blocks of every table in the query.** `duration='2d'` in a macro is `block_number > get_historical_block_number('2 day')` on the bare table. The database works that block out once, when it plans the query.

**2. The newest rows of one contract are cheap over any window.** An index leads to them, `ORDER BY block_number DESC` follows the index, and `LIMIT` stops the scan: 1 to 3 s over 30 days or a year, on the busiest contract.

```sql
SELECT tx_hash(d.block_number, d.tx_index) AS tx, d.from_a AS caller, d.gas, d.gas_used, d.error
FROM {{transaction_detail(duration='30d', only_committed=False)}} d
WHERE d.to_a = 0x0000000071727de22e5e9d8baf0edac6f37da032
  AND selector(d.calldata) = selector('handleOps((address,uint256,bytes,bytes,bytes32,uint256,bytes32,bytes,bytes)[],address)')
ORDER BY d.block_number DESC
LIMIT 20
```

**3. A filter, sort or aggregate on a column outside the index reads every row of the window, so make the window short.** That is any condition on `error`, `gas`, `gas_used`, `vm_step_start` or a decoded argument, any `GROUP BY`, and any `ORDER BY` other than `block_number`. (A filter with `ORDER BY block_number DESC LIMIT` stops early when matches are common; when they are rare it reads the whole window too.) Start with two days: 2 to 7 s for `handleOps` on the EntryPoint (47,000 calls), 7 to 30 s for USDT or WETH (over a million calls). Over 30 days all three take a minute or time out. The first read of a range comes from disk and can take ten times as long as the second, so on a busy contract start with `2h` and widen.

```sql
SELECT d.from_a AS bundler, count(*) AS calls, max(d.gas) AS max_gas, max(d.gas_used) AS max_gas_used,
       count(*) FILTER (WHERE d.error IS NOT NULL) AS failed
FROM transaction_detail d
WHERE d.to_a = 0x0000000071727de22e5e9d8baf0edac6f37da032
  AND selector(d.calldata) = 0x765e827f
  AND d.block_number > get_historical_block_number('2 day')
GROUP BY d.from_a
ORDER BY calls DESC
LIMIT 20
```

Without the `GROUP BY`, `ORDER BY d.gas_used DESC LIMIT 5` over the same rows gives the most expensive calls.

**4. Do not join the big tables to each other, and never join into `transaction_detail`.**

- The hash and the time of a row need no join: `tx_hash(block_number, tx_index)` and `ts(block_number)`.
- For the transaction of each row (sender, gas limit, status), collect the rows first in a materialized CTE ordered by `(block_number, tx_index)`, then join `outer_transaction` on those two columns under the same block bound. A small set is then read by key, and a large one is merged in order: 27 events take 2 s, and 1.6 million take 70 s.

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

- For the frame that made a call (`caller_vm_step_stack[1]` is its step), limit your rows, then read that frame with a scalar subquery on the key. A NULL `caller_error` means the caller went on after the failure.

```sql
SELECT tx_hash(r.block_number, r.tx_index) AS tx, r.to_a AS callee, r.gas, r.gas_used, r.error,
       (SELECT q.error FROM transaction_detail q
        WHERE q.block_number = r.block_number AND q.tx_index = r.tx_index
          AND q.vm_step_start = r.caller_vm_step_stack[1]
          AND q.block_number > get_historical_block_number('2 day')) AS caller_error
FROM (
    SELECT d.block_number, d.tx_index, d.to_a, d.gas, d.gas_used, d.error, d.caller_vm_step_stack
    FROM transaction_detail d
    WHERE d.from_a = 0x0000000071727de22e5e9d8baf0edac6f37da032
      AND d.error IS NOT NULL
      AND d.block_number > get_historical_block_number('2 day')
    ORDER BY d.block_number DESC
    LIMIT 20
) r
```

**5. Transactions sent to a contract: read the top-level frames.** `outer_transaction` has no index on `to_a`: a condition on it reads every transaction of the window, 16 s over two days and a timeout over 30. The frames have the index; the test for step 0 falls under rule 3.

```sql
SELECT tx_hash(d.block_number, d.tx_index) AS tx, d.from_a AS sender, d.gas AS gas_limit, d.gas_used, d.error
FROM {{transaction_detail(duration='30d', only_committed=False)}} d
WHERE d.to_a = 0x4cb67cfee180b6b73b70c7b3fc162e5fcf6532f1
  AND d.vm_step_start = 0
ORDER BY d.block_number DESC
LIMIT 20
```

**6. One transaction takes two queries.** Get `block_number` and `tx_index` from `outer_transaction` by `tx_hash` (the hash has its own index and needs no block bound), then read `transaction_detail` with both as constants. Or run `tx 0xHASH`, which does that.

```sql
SELECT o.block_number, o.tx_index, o.from_a, o.to_a, o.gas, o.gas_used, o.status
FROM outer_transaction o
WHERE o.tx_hash = 0x9c06b464b5007c5c4e579fa9dac4cb3d78211ccbc88a22a93fa791361e28c6ce
```

## When a query fails

- Exit codes: 0 rows printed (possibly none), 1 the server refused or failed the SQL, 2 bad arguments, 3 not logged in, 4 the query slots stayed busy for `--wait`, 5 revoked after `--timeout`.
- After a timeout, shorten the window (rule 3) and check the joins (rule 4) before raising `--timeout`. `explain "SQL"` prints the plan without running the query: a plan that lists dozens of `_hyper_…_chunk` scans has a table without a block bound (rule 1), and a `Hash Join` over `transaction_detail` is rule 4. `expand "SQL"` prints the SQL as the database receives it.
- `status` checks the login and shows how many query slots are in use.
