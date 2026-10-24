//! `Array.prototype.sort` exactly as V8 runs it (`third_party/v8/builtins/array-sort.tq`):
//! TimSort with binary insertion for short runs and galloping merges.
//!
//! Any stable sort agrees with V8 when the comparator is a consistent total
//! order. Ported rankers are not always that: a comparator with an epsilon
//! ("near-ties fall through to the next key") is not transitive, and then the
//! result depends on which pairs the algorithm happens to compare. This port
//! makes the same comparisons in the same order, so the permutation matches.
use std::cmp::Ordering;

const MIN_GALLOP: usize = 7;

/// Sort `v` in place with V8's comparison sequence. `cmp(a, b)` is the JS
/// comparator's sign (`Less` = negative); a NaN result should map to `Equal`.
pub fn js_sort_by<T: Clone>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let mut state = State { min_gallop: MIN_GALLOP, runs: Vec::new() };
    let mut remaining = n;
    let mut low = 0usize;
    let min_run = compute_min_run_length(remaining);
    while remaining != 0 {
        let mut current = count_and_make_run(v, low, low + remaining, &mut cmp);
        if current < min_run {
            let forced = min_run.min(remaining);
            binary_insertion_sort(v, low, low + current, low + forced, &mut cmp);
            current = forced;
        }
        state.runs.push((low, current));
        merge_collapse(v, &mut state, &mut cmp);
        low += current;
        remaining -= current;
    }
    merge_force_collapse(v, &mut state, &mut cmp);
}

/// `js_sort_by` keyed on an `f64` comparator result, the way JS comparators return numbers.
pub fn js_sort_by_num<T: Clone>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> f64) {
    js_sort_by(v, |a, b| {
        let d = cmp(a, b);
        if d < 0.0 {
            Ordering::Less
        } else if d > 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })
}

struct State {
    min_gallop: usize,
    /// (base, length) of each pending run.
    runs: Vec<(usize, usize)>,
}

fn compute_min_run_length(mut n: usize) -> usize {
    let mut r = 0;
    while n >= 64 {
        r |= n & 1;
        n >>= 1;
    }
    n + r
}

fn lt<T>(cmp: &mut impl FnMut(&T, &T) -> Ordering, a: &T, b: &T) -> bool {
    cmp(a, b) == Ordering::Less
}

fn count_and_make_run<T>(v: &mut [T], low_arg: usize, high: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) -> usize {
    let low = low_arg + 1;
    if low == high {
        return 1;
    }
    let mut run_length = 2;
    let descending = lt(cmp, &v[low], &v[low - 1]);
    let mut previous = low;
    let mut idx = low + 1;
    while idx < high {
        let order_lt = lt(cmp, &v[idx], &v[previous]);
        if descending {
            if !order_lt {
                break;
            }
        } else if order_lt {
            break;
        }
        previous = idx;
        run_length += 1;
        idx += 1;
    }
    if descending {
        v[low_arg..low_arg + run_length].reverse();
    }
    run_length
}

fn binary_insertion_sort<T: Clone>(
    v: &mut [T],
    low: usize,
    start_arg: usize,
    high: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    let mut start = if low == start_arg { start_arg + 1 } else { start_arg };
    while start < high {
        let mut left = low;
        let mut right = start;
        let pivot = v[start].clone();
        while left < right {
            let mid = left + ((right - left) >> 1);
            if lt(cmp, &pivot, &v[mid]) {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        let mut p = start;
        while p > left {
            v[p] = v[p - 1].clone();
            p -= 1;
        }
        v[left] = pivot;
        start += 1;
    }
}

fn run_invariant_established(runs: &[(usize, usize)], n: usize) -> bool {
    if n < 2 {
        return true;
    }
    runs[n - 2].1 > runs[n - 1].1 + runs[n].1
}

fn merge_collapse<T: Clone>(v: &mut [T], s: &mut State, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    while s.runs.len() > 1 {
        let mut n = s.runs.len() - 2;
        if !run_invariant_established(&s.runs, n + 1) || !run_invariant_established(&s.runs, n) {
            if s.runs[n - 1].1 < s.runs[n + 1].1 {
                n -= 1;
            }
            merge_at(v, s, n, cmp);
        } else if s.runs[n].1 <= s.runs[n + 1].1 {
            merge_at(v, s, n, cmp);
        } else {
            break;
        }
    }
}

fn merge_force_collapse<T: Clone>(v: &mut [T], s: &mut State, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    while s.runs.len() > 1 {
        let mut n = s.runs.len() - 2;
        if n > 0 && s.runs[n - 1].1 < s.runs[n + 1].1 {
            n -= 1;
        }
        merge_at(v, s, n, cmp);
    }
}

fn merge_at<T: Clone>(v: &mut [T], s: &mut State, i: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    let (mut base_a, mut length_a) = s.runs[i];
    let (base_b, mut length_b) = s.runs[i + 1];
    s.runs[i].1 = length_a + length_b;
    s.runs.remove(i + 1);

    let key_right = v[base_b].clone();
    let k = gallop_right(v, &key_right, base_a, length_a, 0, cmp);
    base_a += k;
    length_a -= k;
    if length_a == 0 {
        return;
    }
    let key_left = v[base_a + length_a - 1].clone();
    length_b = gallop_left(v, &key_left, base_b, length_b, length_b - 1, cmp);
    if length_b == 0 {
        return;
    }
    if length_a <= length_b {
        merge_low(v, s, base_a, length_a, base_b, length_b, cmp);
    } else {
        merge_high(v, s, base_a, length_a, base_b, length_b, cmp);
    }
}

/// Leftmost position in `arr[base..base+length]` where `key` belongs.
fn gallop_left<T>(arr: &[T], key: &T, base: usize, length: usize, hint: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) -> usize {
    let mut last_ofs: isize = 0;
    let mut offset: isize = 1;
    let hint_i = hint as isize;
    if lt(cmp, &arr[base + hint], key) {
        let max_ofs = (length - hint) as isize;
        while offset < max_ofs {
            if !lt(cmp, &arr[base + hint + offset as usize], key) {
                break;
            }
            last_ofs = offset;
            offset = (offset << 1) + 1;
        }
        if offset > max_ofs {
            offset = max_ofs;
        }
        last_ofs += hint_i;
        offset += hint_i;
    } else {
        let max_ofs = hint_i + 1;
        while offset < max_ofs {
            if lt(cmp, &arr[(base as isize + hint_i - offset) as usize], key) {
                break;
            }
            last_ofs = offset;
            offset = (offset << 1) + 1;
        }
        if offset > max_ofs {
            offset = max_ofs;
        }
        let tmp = last_ofs;
        last_ofs = hint_i - offset;
        offset = hint_i - tmp;
    }
    last_ofs += 1;
    while last_ofs < offset {
        let m = last_ofs + ((offset - last_ofs) >> 1);
        if lt(cmp, &arr[base + m as usize], key) {
            last_ofs = m + 1;
        } else {
            offset = m;
        }
    }
    offset as usize
}

/// Rightmost position in `arr[base..base+length]` where `key` belongs.
fn gallop_right<T>(arr: &[T], key: &T, base: usize, length: usize, hint: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) -> usize {
    let mut last_ofs: isize = 0;
    let mut offset: isize = 1;
    let hint_i = hint as isize;
    if lt(cmp, key, &arr[base + hint]) {
        let max_ofs = hint_i + 1;
        while offset < max_ofs {
            if !lt(cmp, key, &arr[(base as isize + hint_i - offset) as usize]) {
                break;
            }
            last_ofs = offset;
            offset = (offset << 1) + 1;
        }
        if offset > max_ofs {
            offset = max_ofs;
        }
        let tmp = last_ofs;
        last_ofs = hint_i - offset;
        offset = hint_i - tmp;
    } else {
        let max_ofs = (length - hint) as isize;
        while offset < max_ofs {
            if lt(cmp, key, &arr[base + hint + offset as usize]) {
                break;
            }
            last_ofs = offset;
            offset = (offset << 1) + 1;
        }
        if offset > max_ofs {
            offset = max_ofs;
        }
        last_ofs += hint_i;
        offset += hint_i;
    }
    last_ofs += 1;
    while last_ofs < offset {
        let m = last_ofs + ((offset - last_ofs) >> 1);
        if lt(cmp, key, &arr[base + m as usize]) {
            offset = m;
        } else {
            last_ofs = m + 1;
        }
    }
    offset as usize
}

#[allow(clippy::too_many_arguments)]
fn merge_low<T: Clone>(
    v: &mut [T],
    s: &mut State,
    base_a: usize,
    length_a_arg: usize,
    base_b: usize,
    length_b_arg: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    let mut length_a = length_a_arg;
    let mut length_b = length_b_arg;
    let temp: Vec<T> = v[base_a..base_a + length_a].to_vec();
    let mut dest = base_a;
    let mut cursor_temp = 0usize;
    let mut cursor_b = base_b;
    v[dest] = v[cursor_b].clone();
    dest += 1;
    cursor_b += 1;

    enum Exit {
        Succeed,
        CopyB,
    }
    let exit = 'outer: {
        length_b -= 1;
        if length_b == 0 {
            break 'outer Exit::Succeed;
        }
        if length_a == 1 {
            break 'outer Exit::CopyB;
        }
        let mut min_gallop = s.min_gallop;
        loop {
            let mut wins_a = 0usize;
            let mut wins_b = 0usize;
            loop {
                if lt(cmp, &v[cursor_b], &temp[cursor_temp]) {
                    v[dest] = v[cursor_b].clone();
                    dest += 1;
                    cursor_b += 1;
                    wins_b += 1;
                    length_b -= 1;
                    wins_a = 0;
                    if length_b == 0 {
                        break 'outer Exit::Succeed;
                    }
                    if wins_b >= min_gallop {
                        break;
                    }
                } else {
                    v[dest] = temp[cursor_temp].clone();
                    dest += 1;
                    cursor_temp += 1;
                    wins_a += 1;
                    length_a -= 1;
                    wins_b = 0;
                    if length_a == 1 {
                        break 'outer Exit::CopyB;
                    }
                    if wins_a >= min_gallop {
                        break;
                    }
                }
            }
            min_gallop += 1;
            let mut first = true;
            while wins_a >= MIN_GALLOP || wins_b >= MIN_GALLOP || first {
                first = false;
                min_gallop = min_gallop.saturating_sub(1).max(1);
                s.min_gallop = min_gallop;
                let key = v[cursor_b].clone();
                wins_a = gallop_right(&temp, &key, cursor_temp, length_a, 0, cmp);
                if wins_a > 0 {
                    for k in 0..wins_a {
                        v[dest + k] = temp[cursor_temp + k].clone();
                    }
                    dest += wins_a;
                    cursor_temp += wins_a;
                    length_a -= wins_a;
                    if length_a == 1 {
                        break 'outer Exit::CopyB;
                    }
                    if length_a == 0 {
                        break 'outer Exit::Succeed;
                    }
                }
                v[dest] = v[cursor_b].clone();
                dest += 1;
                cursor_b += 1;
                length_b -= 1;
                if length_b == 0 {
                    break 'outer Exit::Succeed;
                }
                let key = temp[cursor_temp].clone();
                wins_b = gallop_left(v, &key, cursor_b, length_b, 0, cmp);
                if wins_b > 0 {
                    for k in 0..wins_b {
                        v[dest + k] = v[cursor_b + k].clone();
                    }
                    dest += wins_b;
                    cursor_b += wins_b;
                    length_b -= wins_b;
                    if length_b == 0 {
                        break 'outer Exit::Succeed;
                    }
                }
                v[dest] = temp[cursor_temp].clone();
                dest += 1;
                cursor_temp += 1;
                length_a -= 1;
                if length_a == 1 {
                    break 'outer Exit::CopyB;
                }
            }
            min_gallop += 1;
            s.min_gallop = min_gallop;
        }
    };
    match exit {
        Exit::Succeed => {
            for k in 0..length_a {
                v[dest + k] = temp[cursor_temp + k].clone();
            }
        }
        Exit::CopyB => {
            for k in 0..length_b {
                v[dest + k] = v[cursor_b + k].clone();
            }
            v[dest + length_b] = temp[cursor_temp].clone();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_high<T: Clone>(
    v: &mut [T],
    s: &mut State,
    base_a: usize,
    length_a_arg: usize,
    base_b: usize,
    length_b_arg: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    let mut length_a = length_a_arg as isize;
    let mut length_b = length_b_arg as isize;
    let temp: Vec<T> = v[base_b..base_b + length_b_arg].to_vec();
    let mut dest = (base_b + length_b_arg - 1) as isize;
    let mut cursor_temp = length_b - 1;
    let mut cursor_a = (base_a + length_a_arg - 1) as isize;
    let at = |i: isize| i as usize;
    v[at(dest)] = v[at(cursor_a)].clone();
    dest -= 1;
    cursor_a -= 1;

    enum Exit {
        Succeed,
        CopyA,
    }
    let exit = 'outer: {
        length_a -= 1;
        if length_a == 0 {
            break 'outer Exit::Succeed;
        }
        if length_b == 1 {
            break 'outer Exit::CopyA;
        }
        let mut min_gallop = s.min_gallop;
        loop {
            let mut wins_a = 0isize;
            let mut wins_b = 0isize;
            loop {
                if lt(cmp, &temp[at(cursor_temp)], &v[at(cursor_a)]) {
                    v[at(dest)] = v[at(cursor_a)].clone();
                    dest -= 1;
                    cursor_a -= 1;
                    wins_a += 1;
                    length_a -= 1;
                    wins_b = 0;
                    if length_a == 0 {
                        break 'outer Exit::Succeed;
                    }
                    if wins_a >= min_gallop as isize {
                        break;
                    }
                } else {
                    v[at(dest)] = temp[at(cursor_temp)].clone();
                    dest -= 1;
                    cursor_temp -= 1;
                    wins_b += 1;
                    length_b -= 1;
                    wins_a = 0;
                    if length_b == 1 {
                        break 'outer Exit::CopyA;
                    }
                    if wins_b >= min_gallop as isize {
                        break;
                    }
                }
            }
            min_gallop += 1;
            let mut first = true;
            while wins_a >= MIN_GALLOP as isize || wins_b >= MIN_GALLOP as isize || first {
                first = false;
                min_gallop = min_gallop.saturating_sub(1).max(1);
                s.min_gallop = min_gallop;
                let key = temp[at(cursor_temp)].clone();
                let k = gallop_right(v, &key, base_a, at(length_a), at(length_a - 1), cmp) as isize;
                wins_a = length_a - k;
                if wins_a > 0 {
                    dest -= wins_a;
                    cursor_a -= wins_a;
                    // Copy backwards-safe: source and destination may overlap (dest > cursor_a).
                    for k in (0..wins_a).rev() {
                        v[at(dest + 1 + k)] = v[at(cursor_a + 1 + k)].clone();
                    }
                    length_a -= wins_a;
                    if length_a == 0 {
                        break 'outer Exit::Succeed;
                    }
                }
                v[at(dest)] = temp[at(cursor_temp)].clone();
                dest -= 1;
                cursor_temp -= 1;
                length_b -= 1;
                if length_b == 1 {
                    break 'outer Exit::CopyA;
                }
                let key = v[at(cursor_a)].clone();
                let k = gallop_left(&temp, &key, 0, at(length_b), at(length_b - 1), cmp) as isize;
                wins_b = length_b - k;
                if wins_b > 0 {
                    dest -= wins_b;
                    cursor_temp -= wins_b;
                    for k in 0..wins_b {
                        v[at(dest + 1 + k)] = temp[at(cursor_temp + 1 + k)].clone();
                    }
                    length_b -= wins_b;
                    if length_b == 1 {
                        break 'outer Exit::CopyA;
                    }
                    if length_b == 0 {
                        break 'outer Exit::Succeed;
                    }
                }
                v[at(dest)] = v[at(cursor_a)].clone();
                dest -= 1;
                cursor_a -= 1;
                length_a -= 1;
                if length_a == 0 {
                    break 'outer Exit::Succeed;
                }
            }
            min_gallop += 1;
            s.min_gallop = min_gallop;
        }
    };
    match exit {
        Exit::Succeed => {
            for k in 0..length_b {
                v[at(dest - (length_b - 1) + k)] = temp[at(k)].clone();
            }
        }
        Exit::CopyA => {
            dest -= length_a;
            cursor_a -= length_a;
            for k in (0..length_a).rev() {
                v[at(dest + 1 + k)] = v[at(cursor_a + 1 + k)].clone();
            }
            v[at(dest)] = temp[at(cursor_temp)].clone();
        }
    }
}
