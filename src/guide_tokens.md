# dedaub-monitoring guide: tokens

| Table | One row per | Columns | Indexes |
|---|---|---|---|
| `token_balance` | holder and token, as of now | `token_address`, `owner_address`, `value` (in the token's smallest unit), `block_number` (its last change) | `(token_address, owner_address)`, `(token_address, value)`, `(owner_address)` |
| `token_allowance` | owner, spender and token, as of now | `token_address`, `owner_address`, `spender_address`, `value` | `(token_address, spender_address, owner_address)`, `(owner_address)`, `(spender_address)` |
| `token_ledger` | side of a transfer | `block_number`, `tx_index`, `vm_step_start`, `vm_step`, `address`, `counterparty_address`, `token_address`, `value_delta`, `type` (`ERC20`, `NATIVE`, `SELF_DESTRUCT`) | `(address, block_number, tx_index, vm_step_start, vm_step)`, `(block_number, tx_index, …)`; none on `token_address` |
| `network_token_info` | token | `token_address`, `symbol`, `decimals`, `last_price` (USD for one whole token), `last_cap`, `ts` (when the price was read) | `(token_address)` |

- The native token is `0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee` in all four tables.
- `token_ledger` has two rows for each transfer, one per side. On a row, a positive `value_delta` is what `address` sent to `counterparty_address`, and a negative one what it received. So what an account gained is **minus** the sum of `value_delta` over its rows. The view `token_transfers` is the positive rows, as `from_a`, `to_a` and `value`. The table is split by block like the three history tables: use the macro, `{{token_ledger(duration='2d')}}`, or a block bound.
- Transfers of an account: `token_ledger` by `address`, both directions in one index.
- Transfers of a token: its `Transfer` events in `logs`, where the token is the emitter and the index leads to them; the signature form decodes every event it reads, 5 s for an hour of USDT and 40 s for a day. The ledger has no index on the token: a condition on `token_address` reads every transfer of the window, 5 to 15 s for a day. Native transfers are in the ledger only.
- A native transfer in the ledger is not always one. A `DELEGATECALL` made by a frame that carries ETH inherits its value, and the ledger records that value as sent: a Tornado pool appears to pay its hasher 10 ETH forty times in one deposit. Check an odd ETH flow against the frames (`tx 0xHASH`).
- USD: `to_usd_value(amount, token_address)` for a handful of rows (one lookup per call, NULL for a token without a price). For many rows join `network_token_info n USING (token_address)` and compute `value * n.last_price / 10 ^ n.decimals`. Either way the price is today's.
- Some balances are not money: a token with a supply of 4.5e39 puts one holder at 2e36 USD, at the top of any ranking. In a sum over tokens, cap each position at the token's market cap and leave out tokens without one: `least(value * n.last_price / 10 ^ n.decimals, n.last_cap)` with `n.last_cap > 0`. An ERC-721 transfer is a ledger row of type `ERC20` whose `value_delta` is the token id.
- A first read of a large holder or a widely approved spender can take minutes; the second takes seconds.

The latest transfers of an account, both directions, ordered as the index is:

```sql
SELECT tx_hash(l.block_number, l.tx_index) AS tx, block_timestamp(l.block_number) AS time,
       CASE WHEN l.value_delta > 0 THEN 'sent' ELSE 'received' END AS direction,
       l.counterparty_address, l.token_address, abs(l.value_delta) AS amount,
       to_usd_value(abs(l.value_delta), l.token_address) AS usd
FROM {{token_ledger(duration='30d')}} l
WHERE l.address = 0xd8da6bf26964af9d7eed9e03e53415d37aa96045
ORDER BY l.block_number DESC, l.tx_index DESC, l.vm_step_start DESC, l.vm_step DESC
LIMIT 20
```

Who is exposed to a spender, and for how much. Join allowances to balances first, and the prices to the result.

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

What one account holds is `token_balance b JOIN network_token_info n USING (token_address) WHERE b.owner_address = …`; the largest holders of a token, `token_balance b WHERE b.token_address = … ORDER BY b.value DESC`. `guide incidents` nets flows per address and token.
