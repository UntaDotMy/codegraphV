//! Method names of each language's standard library — names a project type rarely carries itself.
//!
//! A bare `collect` / `String` / `Append` written on some receiver is the standard library's, not
//! the same-named method of whichever project type the name happens to match. Names a project
//! type often carries itself (`get`, `set`, `next`, `Close`, `Value`) are left out: gin's
//! `c.Set(…)` is its Context's.

use std::collections::HashSet;
use std::sync::LazyLock;

fn set(names: &[&'static str]) -> HashSet<&'static str> {
    names.iter().copied().collect()
}

/// Methods of Rust's Option / Result / iterators / collections / strings / smart pointers
/// (and the `std::process::Command` pipes the assert_cmd assertions tests chain onto it).
pub static RUST_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set(&[
        "unwrap", "unwrap_or", "unwrap_or_else", "unwrap_or_default", "unwrap_err", "unwrap_unchecked", "expect",
        "expect_err", "ok", "err", "map", "map_err", "map_or", "map_or_else", "and_then", "or_else", "ok_or",
        "ok_or_else", "is_some", "is_none", "is_ok", "is_err", "is_some_and", "as_ref", "as_mut", "as_deref", "clone",
        "cloned", "copied", "iter", "iter_mut", "into_iter", "collect", "enumerate", "zip", "rev", "chain", "skip",
        "step_by", "peekable", "flat_map", "filter_map", "flatten", "any", "all", "len", "is_empty", "push", "push_str",
        "pop", "extend", "drain", "clear", "retain", "truncate", "reserve", "with_capacity", "capacity", "sort",
        "sort_by", "sort_by_key", "dedup", "split_off", "contains_key", "to_string", "to_owned", "to_vec", "as_str",
        "as_bytes", "as_slice", "as_ptr", "into", "try_into", "borrow", "borrow_mut", "deref", "deref_mut", "chars",
        "bytes", "lines", "starts_with", "ends_with", "trim", "to_lowercase", "to_uppercase", "windows", "chunks",
        "then", "then_some", "eq", "cmp", "partial_cmp", "read_to_end", "read_to_string", "fetch_add", "fetch_sub",
        "current_dir", "stdout", "stderr", "stdin", "success", "failure",
    ])
});

/// Methods of Go's standard types and interfaces — `fmt.Stringer`, `error`,
/// `http.ResponseWriter`, `sync` locks, `reflect`, `time`.
pub static GO_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set(&[
        "String", "Error", "Unwrap", "Is", "As", "Header", "WriteHeader", "WriteString", "Lock", "Unlock", "RLock",
        "RUnlock", "Err", "Deadline", "Int", "Bool", "Float64", "Int64", "Uint64", "Bytes", "Len", "Cap", "Seconds",
        "Unix", "Before", "After", "Equal", "IsNil", "IsValid", "Elem", "NumField", "Interface", "Kind",
    ])
});

pub static KOTLIN_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set(&[
        "apply", "also", "let", "run", "takeIf", "takeUnless", "toString", "equals", "hashCode", "map", "mapNotNull",
        "mapIndexed", "filter", "filterNot", "filterIsInstance", "forEach", "forEachIndexed", "first", "firstOrNull",
        "last", "lastOrNull", "single", "singleOrNull", "isEmpty", "isNotEmpty", "isNullOrEmpty", "isNullOrBlank",
        "isBlank", "isNotBlank", "orEmpty", "joinToString", "toList", "toMutableList", "toSet", "toMutableSet", "toMap",
        "toTypedArray", "any", "all", "none", "count", "sumOf", "maxOf", "minOf", "maxOrNull", "minOrNull", "sortedBy",
        "sortedByDescending", "sorted", "sortedWith", "reversed", "drop", "dropLast", "take", "takeLast", "zip",
        "flatMap", "flatten", "distinct", "groupBy", "associate", "associateBy", "associateWith", "partition",
        "contains", "containsKey", "getOrElse", "getOrNull", "getOrPut", "getOrDefault", "trim", "trimEnd", "trimStart",
        "split", "substring", "startsWith", "endsWith", "replace", "lowercase", "uppercase", "toInt", "toLong",
        "toDouble", "toFloat", "toIntOrNull", "toLongOrNull", "encodeToByteArray", "decodeToString",
        "copyOf", "copyOfRange", "indexOf", "lastIndexOf", "withIndex", "asSequence", "asList", "ifEmpty", "ifBlank",
        "padStart", "padEnd", "repeat", "lines", "toCharArray", "coerceAtLeast", "coerceAtMost", "coerceIn",
    ])
});

const CSHARP_STD: &[&str] = &[
    "ToString", "Equals", "GetHashCode", "GetType", "CompareTo", "Add", "AddRange", "Remove", "RemoveAt", "RemoveAll",
    "Contains", "ContainsKey", "ContainsValue", "Clear", "Insert", "IndexOf", "CopyTo", "ToArray", "ToList",
    "ToDictionary", "GetEnumerator", "MoveNext", "Reset", "TryGetValue", "GetValueOrDefault", "TryAdd", "Write",
    "WriteLine", "WriteAsync", "WriteLineAsync", "Read", "ReadAsync", "ReadLine", "ReadToEnd", "Flush", "FlushAsync",
    "Close", "Dispose", "DisposeAsync", "Parse", "TryParse", "Format", "Join", "Split", "Replace", "Substring", "Trim",
    "TrimStart", "TrimEnd", "StartsWith", "EndsWith", "ToUpper", "ToLower", "ToUpperInvariant", "ToLowerInvariant",
    "Select", "Where", "First", "FirstOrDefault", "Single", "SingleOrDefault", "Last", "LastOrDefault", "Any", "All",
    "Count", "Sum", "Max", "Min", "OrderBy", "OrderByDescending", "GroupBy", "Skip", "Take", "Distinct", "Concat",
    "Cast", "OfType", "Aggregate", "Invoke", "DynamicInvoke", "GetMethod", "GetProperty", "GetField", "GetConstructor",
    "GetCustomAttributes", "GetGenericArguments", "MakeGenericType", "IsAssignableFrom", "ConfigureAwait", "Wait",
    "ContinueWith", "Append", "AppendLine", "Peek", "Push", "Pop", "Enqueue", "Dequeue", "HasFlag", "Find", "FindAll",
    "ForEach", "Sort", "Reverse", "Clone", "Seek", "SetLength",
];

pub static CSHARP_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| set(CSHARP_STD));

/// The C# set, lowercased: VB.NET folds case.
pub static VBNET_STD_METHODS: LazyLock<HashSet<String>> = LazyLock::new(|| CSHARP_STD.iter().map(|m| m.to_lowercase()).collect());

pub static DART_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set(&[
        "endsWith", "startsWith", "contains", "split", "substring", "trim", "trimLeft", "trimRight", "toLowerCase",
        "toUpperCase", "replaceAll", "replaceFirst", "replaceRange", "indexOf", "lastIndexOf", "padLeft", "padRight",
        "codeUnitAt", "allMatches", "firstMatch", "hasMatch", "addAll", "removeAt", "removeWhere", "removeLast",
        "retainWhere", "insertAll", "where", "whereType", "forEach", "toList", "toSet", "join", "reduce", "fold",
        "any", "every", "firstWhere", "lastWhere", "singleWhere", "containsKey", "containsValue", "putIfAbsent",
        "sublist", "take", "takeWhile", "skip", "skipWhile", "expand", "cast", "compareTo", "elementAt", "followedBy",
        "asMap", "getRange", "setAll", "fillRange", "shuffle", "sort", "indexWhere", "lastIndexWhere",
    ])
});

pub static SWIFT_STD_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    set(&[
        "append", "insert", "remove", "removeAll", "removeFirst", "removeLast", "removeValue", "contains", "map",
        "compactMap", "flatMap", "filter", "reduce", "forEach", "sorted", "sort", "first", "last", "min", "max",
        "firstIndex", "lastIndex", "index", "enumerated", "reversed", "joined", "split", "prefix", "suffix",
        "dropFirst", "dropLast", "allSatisfy", "randomElement", "shuffled", "popLast", "replaceSubrange",
        "replacingOccurrences", "components", "trimmingCharacters", "hasPrefix", "hasSuffix", "lowercased",
        "uppercased", "appending", "updateValue", "merge", "merging", "union", "intersection", "subtracting",
        "formUnion", "isEqual", "addSubview", "removeFromSuperview", "setNeedsDisplay", "setNeedsLayout",
        "layoutIfNeeded", "addGestureRecognizer", "addTarget", "addObserver", "removeObserver", "eraseToAnyPublisher",
    ])
});

/// `isStdMethodName(language, name)`: is `name` a standard-library method of `language`?
pub fn is_std_method_name(language: &str, name: &str) -> bool {
    match language {
        "go" => GO_STD_METHODS.contains(name),
        "rust" => RUST_STD_METHODS.contains(name),
        "kotlin" => KOTLIN_STD_METHODS.contains(name),
        "csharp" => CSHARP_STD_METHODS.contains(name),
        "vbnet" => VBNET_STD_METHODS.contains(js_lower(name).as_str()),
        "dart" => DART_STD_METHODS.contains(name),
        _ => false,
    }
}

fn js_lower(s: &str) -> String {
    crate::ctx::js_lower(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_sets_are_consulted_by_language() {
        assert!(is_std_method_name("rust", "collect"));
        assert!(!is_std_method_name("rust", "String"));
        assert!(is_std_method_name("go", "String"));
        assert!(!is_std_method_name("go", "collect"));
        assert!(is_std_method_name("vbnet", "WRITELINE"));
        assert!(!is_std_method_name("csharp", "WRITELINE"));
        assert!(!is_std_method_name("python", "len"));
    }
}
