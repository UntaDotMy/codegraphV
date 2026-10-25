| repo | arm | queries | recall | located | wall ms (med) | cpu ms (med) | peak RSS MB | tokens (med) |
|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 11 | 1.00 | 1.00 | 37 | 31 | 10 | 696 |
| serde_json | ts | 11 | 0.94 | 0.94 | 1102 | 984 | 190 | 3367 |
| serde_json | rg | 11 | 0.88 | 0.88 | 104 | 156 | 8 | 28970 |
| tokio | rust | 11 | 0.92 | 0.92 | 50 | 47 | 18 | 2433 |
| tokio | ts | 11 | 0.80 | 0.80 | 1217 | 1156 | 211 | 6239 |
| tokio | rg | 11 | 0.64 | 0.64 | 218 | 828 | 8 | 63735 |
| codegraph | rust | 12 | 1.00 | 1.00 | 57 | 47 | 24 | 2376 |
| codegraph | ts | 12 | 0.74 | 0.94 | 1976 | 1859 | 328 | 6236 |
| codegraph | rg | 12 | 0.71 | 0.71 | 475 | 2688 | 32 | 63150 |

| repo | engine | files | nodes | edges | index wall ms | index cpu ms | avg cores | peak RSS MB | peak commit MB |
|---|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 69 | 2100 | 6135 | 1859 | 2125 | 1.14 | 53 | 101 |
| serde_json | ts | 69 | 2100 | 6135 | 4746 | 9984 | 2.10 | 486 | 902 |
| tokio | rust | 555 | 9934 | 25384 | 9103 | 11297 | 1.24 | 156 | 309 |
| tokio | ts | 555 | 9934 | 25384 | 16311 | 47766 | 2.93 | 1120 | 1253 |
| codegraph | rust | 1869 | 49190 | 179836 | 54945 | 117984 | 2.15 | 1144 | 2136 |
| codegraph | ts | 1869 | 49190 | 179836 | 52442 | 229719 | 4.38 | 3928 | 4009 |
