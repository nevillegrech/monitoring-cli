# dedaub-monitoring guide: old data

Blocks older than 15 days are compressed sooner or later, and a query that takes a second on last week's blocks can time out on last year's. This page says which reads still work there, on any chain. Times are from Ethereum.

## What a chain has

`dedaub-monitoring coverage --chain base` prints a row per table:

- `uncompressed_from_block`: from this block on, every index of `indexed` works. Older blocks are compressed.
- `indexed_compressed`: the order compressed rows are stored in. Its first column is the contract: `to_a` for frames, `address` for logs and `token_ledger`.
- `filtered_compressed`: the columns compressed rows can be skipped by when the contract is not given. Every chain has `from_a` for frames; few have `topic0` for logs.
- `address_selector`, where the chain has that table, with the first block it holds. A note says so when the API cannot read it there.

## What a read costs in compressed blocks

Compressed rows lie in batches of up to 1,000 rows of one contract.

| The query has a condition on | The database reads | Time |
|---|---|---|
| the first column of `indexed_compressed` | that contract's batches, newest first | seconds over any window (rule 2 of `guide`) |
| a column of `filtered_compressed` only | an entry for every batch of the window, then the batches that may hold the value | a rare `topic0`: 13 s for a year (74 s on Arbitrum). `from_a`: 30 s for a window that reaches one month into the compressed blocks, a timeout at three |
| neither | every row of the window | a timeout |

A filter skips batches, so it works for a value that few contracts hold. `Transfer` is in the batches of 80% of all events, and `RoleGranted(bytes32,address,address)` sets the same filter bits as `Transfer`: give the contract for both.

## Without the contract

**Transactions that involve a selector or an address.** `address_selector` has a row for each code a transaction involves: a selector called at any depth, or an address that called, was called, or sent or received a token. It is stored by code, so any window is cheap: 2 s for the newest rows of a year, 6 s to count 80,000 transactions by month.

```sql
SELECT tx_hash(s.block_number, s.tx_index) AS tx, block_timestamp(s.block_number) AS time
FROM address_selector s
WHERE s.selector = (encode_adresses_selectors(ARRAY[0x2f2ff15d]))[1]
  AND s.block_number > get_historical_block_number('365 day')
ORDER BY s.block_number DESC, s.tx_index DESC
```

`encode_adresses_selectors` gives the code of each selector or address in the array. A code has 32 bits and an address is hashed into it, so now and then another address shares it: confirm on what you read next. For two conditions, `INTERSECT` two such queries that select `(block_number, tx_index)`: the `grantRole` transactions that involve one busy contract take 3 s. Then read each transaction with `tx 0xHASH`.

**Calls a contract made.** Take its callees from the uncompressed blocks, then read each pair: `subcalls 0xCONTRACT --count-by callee --last 15d`, then `subcalls 0xCONTRACT --to 0xCALLEE --last 365d` (7 s). A callee it no longer calls is missed: `address_selector` by the contract's address lists its transactions of that time.

**A function on any contract.** `contracts 'f(uint256)'` lists the contracts that have it, and `calls 0xCONTRACT 'f(uint256)' --last 365d` reads each. Or start from `address_selector`.

**An event on any contract.** Where `filtered_compressed` of `logs` has `topic0`, a rare topic alone works over a year. Where it has not, `indexed` has no `topic0` either: a topic alone reads every event of the window, so keep that to a day or name the emitters.

**One old transaction.** `tx 0xHASH` reads it from the platform's trace, which lacks the gas each frame was given. In SQL, the frames of a transaction in compressed blocks take 20 s or more, with its block and index as constants too: do not loop over transactions that way.

**A token's transfers.** `logs` with the token as emitter (`guide tokens`). `token_ledger` is stored by account: an account's transfers are cheap over any window, a token's are not.
