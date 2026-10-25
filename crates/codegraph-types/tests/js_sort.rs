//! `js_sort_by` reproduces V8's permutation even for inconsistent comparators
//! (vectors from `scripts/rust-golden/js-sort.mjs`).
use codegraph_types::jssort::js_sort_by_num;
use serde_json::Value;

fn comparator(name: &str) -> fn(&(i64, i64, usize), &(i64, i64, usize)) -> f64 {
    match name {
        "eps" => |a, b| if (a.0 - b.0).abs() > 30 { (b.0 - a.0) as f64 } else { (a.1 - b.1) as f64 },
        "chaos" => |a, b| ((a.0 * 7 + b.0 * 13) % 5 - 2) as f64,
        "total" => |a, b| (a.1 - b.1) as f64,
        "runs" => |a, b| (a.0 - b.0) as f64,
        "epsRuns" => |a, b| if (a.0 - b.0).abs() > 2 { (a.0 - b.0) as f64 } else { (b.1 - a.1) as f64 },
        other => panic!("unknown comparator {other}"),
    }
}

#[test]
fn js_sort_matches_v8() {
    let mut failures = 0;
    let mut total = 0;
    for line in include_str!("fixtures/js_sort.jsonl").lines().filter(|l| !l.is_empty()) {
        total += 1;
        let v: Value = serde_json::from_str(line).unwrap();
        let mut items: Vec<(i64, i64, usize)> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, p)| (p[0].as_i64().unwrap(), p[1].as_i64().unwrap(), i))
            .collect();
        let cmp = comparator(v["cmp"].as_str().unwrap());
        js_sort_by_num(&mut items, cmp);
        let order: Vec<usize> = items.iter().map(|x| x.2).collect();
        let expected: Vec<usize> = v["order"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as usize).collect();
        if order != expected {
            failures += 1;
            if failures <= 3 {
                eprintln!("{} n={} first diff at {:?}", v["cmp"], expected.len(), order.iter().zip(&expected).position(|(a, b)| a != b));
            }
        }
    }
    assert_eq!(failures, 0, "{failures}/{total} permutations differ from V8");
}
