| repo | arm | queries | recall | located | wall ms (med) | cpu ms (med) | peak RSS MB | tokens (med) |
|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 11 | 1.00 | 1.00 | 35 | 31 | 10 | 699 |
| serde_json | ts | 11 | 0.94 | 0.94 | 1012 | 953 | 188 | 3367 |
| serde_json | rg | 11 | 0.88 | 0.88 | 93 | 172 | 8 | 28501 |
| tokio | rust | 11 | 0.92 | 0.92 | 47 | 31 | 18 | 2433 |
| tokio | ts | 11 | 0.80 | 0.80 | 1145 | 1078 | 211 | 6239 |
| tokio | rg | 11 | 0.48 | 0.48 | 213 | 719 | 8 | 50513 |
| codegraph | rust | 12 | 1.00 | 1.00 | 53 | 47 | 24 | 2336 |
| codegraph | ts | 12 | 0.74 | 0.94 | 1909 | 1820 | 328 | 6240 |
| codegraph | rg | 12 | 0.51 | 0.51 | 458 | 2422 | 32 | 60749 |

| repo | engine | files | nodes | edges | index wall ms | index cpu ms | avg cores | peak RSS MB | peak commit MB |
|---|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 69 | 2100 | 6134 | 1636 | 1938 | 1.18 | 53 | 100 |
| serde_json | ts | 69 | 2100 | 6135 | 4151 | 9094 | 2.19 | 484 | 906 |
| tokio | rust | 555 | 9934 | 25538 | 8417 | 10562 | 1.25 | 155 | 307 |
| tokio | ts | 555 | 9934 | 25384 | 15290 | 45453 | 2.97 | 1117 | 1257 |
| codegraph | rust | 1819 | 47492 | 170653 | 45804 | 93016 | 2.03 | 959 | 1912 |
| codegraph | ts | 1819 | 47521 | 170782 | 48842 | 215047 | 4.40 | 3889 | 3969 |
