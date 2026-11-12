---
name: dedaub-monitoring
description: >
  Query the on-chain history of Ethereum and other EVM chains (call frames, events,
  transactions, storage changes) and read contract code with the dedaub-monitoring CLI. Use
  it to find who calls a function and with how much gas, which calls fail and whether the
  caller goes on, what a contract emitted, what one transaction did, or whether it still works
  under the next fork's gas rules; to read a contract's source, or its decompiled code and ABI
  when it has no verified source; to find contracts by their functions, name or deployer; and
  to get token balances, transfers and allowances, with USD values.
---

# dedaub-monitoring

Read-only queries over Dedaub's index of EVM chains. Rows print as TSV, exact and uncut.

1. Check access once: `dedaub-monitoring status`. Exit code 3 means not logged in: say so and
   stop, a retry will not help.
2. Start with a lookup. Each returns in seconds, with decoded arguments:
   - `calls 0xCONTRACT [FUNCTION]`, `subcalls 0xCONTRACT`: `--failed`, `--top-level`,
     `--tolerated`, `--returns`, `--tx` (the call's transaction); `--count-by function`
     (or `caller`, `callee`, `status`) counts instead of listing
   - `events 0xCONTRACT [EVENT]`, `tx 0xHASH`, `replay 0xHASH`, `state 0xHASH`
   - `code 0xCONTRACT` (source, or decompiled code when unverified), `abi 0xCONTRACT`
   - `contracts 'f(uint256)' …`, `--name TEXT`, `--deployer 0xADDRESS`, `--same-code 0xCONTRACT`
   - `sig NAME|0xSELECTOR|0xTOPIC0`, `schema [TABLE]`

   `calls`, `subcalls` and `events` take `--last 30d --limit 20`. Over fifty rows, write them
   to a file (`> rows.tsv`; notes stay on stderr) and work on the file.
3. For any other question write SQL: `dedaub-monitoring sql "SELECT …"`. First read all of
   `dedaub-monitoring guide` (100 lines). Its other pages: `guide contracts`, `guide tokens`,
   `guide incidents`.

## Rules for SQL

- Bound the blocks of every table: `duration='2d'` in a macro, or
  `block_number > get_historical_block_number('2 day')` on a bare table.
- The newest rows of a contract (`ORDER BY block_number DESC LIMIT n`) are cheap over any
  window. A filter, sort or aggregate on a column outside the index reads the whole window:
  use two days or less.
- Never join `transaction_detail` with `logs`, `outer_transaction`, `token_ledger` or itself.
  The hash is `tx_hash(block_number, tx_index)`, the time `block_timestamp(block_number)`.
  For other transaction fields, collect the rows in a materialized CTE ordered by
  `(block_number, tx_index)`, then join `outer_transaction` on those two columns, with the
  same block bound on it.
- Read arguments, results and events with the signature form of the macros, where each
  named argument is a column: `{{logs('0xCONTRACT.Transfer(address src, address dst, uint256 wad)')}}`.
- Write addresses, selectors and topics bare (`0x…`, no quotes), and no chain prefix.

Run the commands plainly, without sleep or retry loops: they queue by themselves for the
account's three query slots. `--chain` takes a chain name or id; the default is ethereum.
