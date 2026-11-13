//! JS/TS runtime names — port of `src/resolution/js-builtins.ts`.

use std::collections::HashSet;
use std::sync::LazyLock;

pub static JS_BUILT_INS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["console", "window", "document", "global", "process", "Promise", "Array", "Object", "String", "Number", "Boolean", "Date", "Math", "JSON", "RegExp", "Error", "Map", "Set", "WeakMap", "WeakSet", "setTimeout", "setInterval", "clearTimeout", "clearInterval", "fetch", "require", "module", "exports", "__dirname", "__filename"].into_iter().collect()
});

/// The browser host's own objects: a member call on one is a DOM call, not a project method.
pub static JS_HOST_GLOBALS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["window", "globalThis", "global", "document", "localStorage", "sessionStorage", "location", "history", "navigator", "self"]
        .into_iter()
        .collect()
});

/// `window.<member>(...)` calls the host itself, unless the project hung an object on `window`.
pub static JS_WINDOW_MEMBERS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["open", "close", "postMessage", "alert", "confirm", "prompt", "focus", "blur", "print", "scroll", "scrollTo", "scrollBy", "getComputedStyle", "matchMedia", "requestAnimationFrame", "cancelAnimationFrame", "setTimeout", "setInterval", "clearTimeout", "clearInterval", "addEventListener", "removeEventListener", "dispatchEvent", "getSelection", "fetch"]
        .into_iter()
        .collect()
});

pub static JS_BUILTIN_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["at", "concat", "copyWithin", "entries", "every", "fill", "filter", "find", "findIndex", "findLast", "findLastIndex", "flat", "flatMap", "forEach", "includes", "indexOf", "join", "keys", "lastIndexOf", "map", "pop", "push", "reduce", "reduceRight", "reverse", "shift", "slice", "some", "sort", "splice", "toReversed", "toSorted", "toSpliced", "unshift", "values", "with", "subarray", "get", "set", "has", "add", "delete", "clear", "charAt", "charCodeAt", "codePointAt", "endsWith", "localeCompare", "match", "matchAll", "normalize", "padEnd", "padStart", "repeat", "replace", "replaceAll", "search", "split", "startsWith", "substring", "substr", "toLowerCase", "toUpperCase", "toLocaleLowerCase", "toLocaleUpperCase", "trim", "trimStart", "trimEnd", "trimLeft", "trimRight", "toString", "toLocaleString", "valueOf", "then", "catch", "finally", "call", "apply", "bind", "addEventListener", "removeEventListener", "dispatchEvent", "on", "once", "off", "emit", "addListener", "removeListener", "removeAllListeners", "prependListener", "prependOnceListener", "listeners", "rawListeners", "listenerCount", "eventNames", "setMaxListeners", "getMaxListeners", "next", "return", "throw", "drop", "take", "toArray", "text", "json", "arrayBuffer", "blob", "formData"].into_iter().collect()
});

pub static JS_ARRAY_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["at", "concat", "copyWithin", "entries", "every", "fill", "filter", "find", "findIndex", "findLast", "findLastIndex", "flat", "flatMap", "forEach", "includes", "indexOf", "join", "keys", "lastIndexOf", "map", "pop", "push", "reduce", "reduceRight", "reverse", "shift", "slice", "some", "sort", "splice", "toReversed", "toSorted", "toSpliced", "unshift", "values", "with", "toString", "toLocaleString", "valueOf", "hasOwnProperty", "isPrototypeOf", "propertyIsEnumerable"].into_iter().collect()
});

pub static TS_PRIMITIVE_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["string", "number", "boolean", "bigint", "symbol", "void", "undefined", "null", "never", "unknown", "any", "object"].into_iter().collect()
});
