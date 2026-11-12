# dedaub-monitoring guide: contracts and their code

## The lookups

- `code 0xCONTRACT` prints the verified source: the files with code in the contract, the main one first (`--files` lists them all, `--file Gateway.sol` prints one, `--all` every one). Without verified source it prints the decompiled code (`--decompiled` asks for that in any case). For a proxy it prints the implementation's code; `--proxy` gives the proxy's own.
- `contracts` lists contracts newest first. Its arguments are functions the bytecode must have, as signatures or selectors: it finds verified and unverified contracts alike, in 6 to 15 s, or in 1 s when the functions are common. `--deployer 0xADDRESS` (the account that sent the creating transaction, or the factory), `--same-code 0xCONTRACT`, `--no-source` and `--last 30d` narrow it or stand alone. `--same-code` lists the contracts with that bytecode and with the bytecodes that differ from it only in constants (`same_bytecode` marks the exact copies): the other deployments of a contract with immutables. A proxy has only its own few functions, so a search by functions finds implementations. `--show-sql` prints the search.
- `contracts --name TEXT` asks the platform's name search instead: its 30 best matches, under the platform's display name. It misses some exact names; a search by the contract's functions finds those.

## The tables

These tables are not split by block and need no block bound.

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `contracts` | contract | `address`, `md5_bytecode` (the code id: equal values are the same bytecode), `block_number` and `tx_index` (its creation), `deployer` (the address that created it: an account or a factory), `eoa_deployer` (the account that sent that transaction), `block_number_destroyed` | `(address)`, `(md5_bytecode)`, `(deployer)`, `(eoa_deployer)`, `(block_number)` |
| `decompiled_code` | bytecode, verified or not | `md5_bytecode`, `array_function_selectors`, `array_event_selectors` (topic0 values), `source_level` (the decompiled code) | `(md5_bytecode)`; none on the selectors |
| `code` | bytecode with verified source | `md5_bytecode`, `contract_name`, `compiler`, `json_abi`, `array_function_signatures` | `(md5_bytecode)` |
| `normalised_md5` | bytecode | `md5_bytecode`, `normalised_md5` (equal for bytecodes that differ only in their constants) | `(md5_bytecode)`, `(normalised_md5)` |
| `proxy_implementation_latest` | proxy | `proxy_address`, `implementation_address`, `transitive_address`, `beacon_address`, `proxy_type` (`MINIMAL`, `EIP1967`, `UUPS`, `BEACON`, …), `last_upgrade_block` | each of the four addresses |

- Search by functions in `decompiled_code`: it has every bytecode's functions, 3 million rows on Ethereum and no index on the selectors, so a search reads them all.
- Read `code` by `md5_bytecode` only, as a join or a subquery. A condition on its name or its selectors reads all 13 million rows: 30 s for an exact name, a timeout for a pattern.
- `normalised_md5` groups the copies of one contract that were deployed with different constants. It says nothing for a stub: a runtime of 68 bytes shares its value with 1,500 unrelated ones.
- The code a proxy runs is at `implementation_address`, or behind a beacon at `transitive_address`: match both. For one beacon-backed implementation the first finds 5,193 proxies and the second 96,694.
- The proxy table against the chain, on 1,684 sampled rows (November 2026): of 1,583 proxies for which the chain names an implementation, the table has the same one for 1,495, another for 4 and none for 84. A third of the Safe and ERC-7579 account rows are empty.
- The planner misjudges these tables, so hand it a set as an array (rule 8): `guide incidents` has the query.
