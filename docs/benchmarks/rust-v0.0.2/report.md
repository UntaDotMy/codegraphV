| repo | arm | queries | recall | located | wall ms (med) | cpu ms (med) | peak RSS MB | tokens (med) |
|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 11 | 1.00 | 1.00 | 37 | 31 | 12 | 696 |
| serde_json | ts | 11 | 0.94 | 0.94 | 1039 | 969 | 190 | 3367 |
| serde_json | rg | 11 | 0.88 | 0.88 | 94 | 94 | 8 | 28970 |
| tokio | rust | 11 | 0.92 | 0.92 | 48 | 47 | 20 | 2433 |
| tokio | ts | 11 | 0.80 | 0.80 | 1175 | 1109 | 211 | 6239 |
| tokio | rg | 11 | 0.39 | 0.39 | 215 | 828 | 8 | 58016 |
| codegraph | rust | 12 | 1.00 | 1.00 | 53 | 47 | 26 | 2632 |
| codegraph | ts | 12 | 0.74 | 0.94 | 1949 | 1828 | 332 | 6235 |
| codegraph | rg | 12 | 0.51 | 0.51 | 462 | 2719 | 33 | 52987 |

| repo | engine | files | nodes | edges | index wall ms | index cpu ms | avg cores | peak RSS MB | peak commit MB |
|---|---|---|---|---|---|---|---|---|---|
| serde_json | rust | 69 | 2100 | 6135 | 1119 | 1594 | 1.42 | 68 | 201 |
| serde_json | ts | 69 | 2100 | 6135 | 4070 | 9469 | 2.33 | 480 | 904 |
| tokio | rust | 555 | 9934 | 25384 | 5208 | 7859 | 1.51 | 175 | 434 |
| tokio | ts | 555 | 9934 | 25384 | 15258 | 46219 | 3.03 | 1110 | 1253 |
| codegraph | rust | 1878 | 49367 | 180304 | 30937 | 120500 | 3.90 | 2126 | 3391 |
| codegraph | ts | 1878 | 49367 | 180304 | 51021 | 225703 | 4.42 | 3976 | 4047 |
