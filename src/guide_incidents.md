# dedaub-monitoring guide: an incident

When a contract is drained, three questions come first: who lost what, who else runs the same code, and where the money went. Each has a query here that runs in seconds.

The examples are the drain of a market maker on 7 May 2026 by the account `0xc3eb…9100` (an attacker's account is the `eoa_deployer` of the exploit contract in `contracts`). They are months old, so they bound the blocks with a number; in a live incident write `> get_historical_block_number('2 day')`.

## Who gained and who lost

Every token movement inside the attacker's transactions, netted per address and token.

```sql
WITH a AS MATERIALIZED (
    SELECT o.block_number, o.tx_index
    FROM outer_transaction o
    WHERE o.from_a = 0xc3ebddea4f69df717a8f5c89e7cf20c1c0389100
      AND o.block_number BETWEEN 25039000 AND 25100000
    ORDER BY o.block_number, o.tx_index
)
SELECT x.address,
       (SELECT k.contract_name FROM contracts c JOIN code k USING (md5_bytecode) WHERE c.address = x.address) AS name,
       n.symbol, x.transfers, x.net / 10 ^ n.decimals AS net,
       round((x.net * n.last_price / 10 ^ n.decimals)::numeric) AS usd_now
FROM (
    SELECT l.address, l.token_address, count(*) AS transfers, -sum(l.value_delta) AS net
    FROM token_ledger l
    JOIN a USING (block_number, tx_index)
    WHERE l.block_number BETWEEN 25039000 AND 25100000
    GROUP BY l.address, l.token_address
    HAVING sum(l.value_delta) <> 0
) x
JOIN network_token_info n USING (token_address)
ORDER BY abs(x.net * n.last_price / 10 ^ n.decimals) DESC NULLS LAST
LIMIT 40
```

- The transactions are collected first, in order of position, and the ledger is joined on `(block_number, tx_index)` under the same block bound: 3 s for 13 transactions. The victim shows at −1,291 WETH, −1,268,771 USDC, −16.9 WBTC and −206,282 USDT, the figures that were published.
- For one transaction no CTE is needed: `WHERE (l.block_number, l.tx_index) = (SELECT o.block_number, o.tx_index FROM outer_transaction o WHERE o.tx_hash = 0x…)`.
- Read the ETH rows with care (`guide tokens`): here the ledger shows Tornado's hasher receiving 404 ETH that never moved.

## Who else runs the code, and what does it hold

The contracts whose bytecode equals the vulnerable one up to its constants, and the proxies in front of them, ranked by what they hold. The example is Aave's aToken implementation.

```sql
WITH code AS MATERIALIZED (
    SELECT c.address
    FROM contracts c
    WHERE c.md5_bytecode = ANY (ARRAY(
              SELECT m.md5_bytecode::bytea
              FROM contracts x
              JOIN normalised_md5 v USING (md5_bytecode)
              JOIN normalised_md5 m USING (normalised_md5)
              WHERE x.address = 0xadc45df3cf1584624c97338bef33363bf5b97ada))
      AND c.block_number_destroyed IS NULL
), holder AS MATERIALIZED (
    SELECT address FROM code
  UNION
    SELECT p.proxy_address
    FROM proxy_implementation_latest p
    WHERE p.implementation_address = ANY (ARRAY(SELECT address::bytea FROM code))
       OR p.transitive_address = ANY (ARRAY(SELECT address::bytea FROM code))
)
SELECT h.address AS holder, count(*) AS tokens,
       round(sum(least(b.value * n.last_price / 10 ^ n.decimals, n.last_cap))::numeric) AS usd,
       (array_agg(n.symbol ORDER BY b.value * n.last_price / 10 ^ n.decimals DESC))[1:3] AS largest
FROM holder h
JOIN token_balance b ON b.owner_address = h.address
JOIN network_token_info n USING (token_address)
WHERE b.value > 0
  AND n.last_cap > 0
GROUP BY h.address
ORDER BY usd DESC NULLS LAST
LIMIT 25
```

- `normalised_md5` joined to itself gives the bytecodes of the family: here 10, among them a second implementation. The result is the aToken markets, from weETH at 3.1 billion USD down.
- Each set goes in as an array (rule 8). Written with plain joins the query timed out at 100 s: the planner chose to scan all 60 million proxies. As written it takes 3 to 15 s.
- A proxy runs the code at `implementation_address` or, behind a beacon, at `transitive_address`: match both.
- A position is capped at the token's market cap, or worthless tokens with made-up prices lead every sum.

## Where the money went

A walk forward from the attacker's addresses, one hop at a time, three hops deep.

```sql
WITH RECURSIVE node AS (
    SELECT 0 AS hop, s.a AS address, 25039670::bigint AS since, true AS follow
    FROM (VALUES (0xc3ebddea4f69df717a8f5c89e7cf20c1c0389100), (0xd4d5db5ec65272b26f756712247281515f211e95)) s(a)
  UNION
    SELECT t.hop + 1, o.to_a, o.since,
           (SELECT count(*) FROM (
                SELECT 1 FROM token_ledger q
                WHERE q.address = o.to_a AND q.block_number > 25030000
                  AND q.block_number BETWEEN o.since - 7200 AND o.since - 1
                LIMIT 51) busy) <= 50
    FROM node t
    CROSS JOIN LATERAL (
        SELECT l.counterparty_address AS to_a, min(l.block_number) AS since
        FROM token_ledger l
        JOIN network_token_info n USING (token_address)
        WHERE l.address = t.address AND l.block_number > 25030000 AND l.block_number >= t.since
          AND l.value_delta > 0
        GROUP BY l.counterparty_address
        HAVING sum(l.value_delta * n.last_price / 10 ^ n.decimals) > 10000
    ) o
    WHERE t.follow AND t.hop < 2
), followed AS MATERIALIZED (
    SELECT address, min(hop) AS hop, min(since) AS since FROM node WHERE follow GROUP BY address
)
SELECT t.hop + 1 AS hop, t.address AS from_a, e.to_a,
       CASE WHEN EXISTS (SELECT 1 FROM contracts c WHERE c.address = e.to_a) THEN 'contract' ELSE 'account' END AS kind,
       (SELECT string_agg(DISTINCT cl.list_name, ', ') FROM listed_contracts lc JOIN contract_list cl USING (list_id)
        WHERE lc.address = e.to_a
          AND cl.list_name IN ('exchange', 'Deposit Address', 'Kraken', 'Coinbase', 'Binance',
                               'tornado_cash_addresses', 'hack-phish-related')) AS labels,
       n.symbol, e.transfers, e.amount / 10 ^ n.decimals AS amount,
       round((e.amount * n.last_price / 10 ^ n.decimals)::numeric) AS usd_now, block_timestamp(e.first_block) AS first_time
FROM followed t
CROSS JOIN LATERAL (
    SELECT l.counterparty_address AS to_a, l.token_address, count(*) AS transfers, sum(l.value_delta) AS amount,
           min(l.block_number) AS first_block
    FROM token_ledger l
    WHERE l.address = t.address AND l.block_number > 25030000 AND l.block_number >= t.since
      AND l.value_delta > 0
    GROUP BY l.counterparty_address, l.token_address
) e
JOIN network_token_info n ON n.token_address = e.token_address
WHERE e.amount * n.last_price / 10 ^ n.decimals > 10000
ORDER BY hop, usd_now DESC
LIMIT 80
```

- `node` is the accounts the money passed through, each with the block it was first paid at; only its transfers from that block on are read. The second part lists what they sent.
- An account is followed only if it was quiet before it was paid: at most 50 ledger rows in the 7,200 blocks (a day) before. A busy one is an exchange wallet, a bridge or a pool, where the money mixes with everyone's: it is shown as a receiver and the walk stops there. Here the attacker's own accounts had 0 rows in the day before, and the services between 87 and 5,389.
- Only a receiver of 10,000 USD or more is followed, so dust sent to bystanders does not pull them in.
- Labels come last, on the rows that are printed, from the public lists in `contract_list` and `listed_contracts`. Name the lists: many public ones were made by users.
- 7 to 10 s once the accounts' rows have been read, 30 to 45 s the first time. Without the test for a quiet account the walk timed out.
