//! Port of `__tests__/dynamic-boundaries.test.ts`'s scanner assertions (#687,
//! #1967). The explore renderer is not ported yet; this pins the verdict it
//! renders.
use codegraph_graph::dynamic_boundaries::scan_dynamic_dispatch;

fn forms(body: &str) -> Vec<String> {
    scan_dynamic_dispatch(body, "typescript", 1)
        .into_iter()
        .map(|m| m.form)
        .collect()
}

#[test]
fn detects_a_computed_member_call_with_a_literal_key() {
    let body = "function go(p) {\n  table['save'](p);\n}";
    let m = scan_dynamic_dispatch(body, "typescript", 10);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "computed-call");
    assert_eq!(m[0].key.as_deref(), Some("save"));
    assert_eq!(m[0].line, 11); // absolute: body starts at file line 10
    assert!(m[0].snippet.contains("table['save'](p)"));
}

#[test]
fn detects_a_computed_member_call_with_a_runtime_key() {
    let body = "dispatch(action) {\n  this.handlers[action.type](action.payload);\n}";
    let m = scan_dynamic_dispatch(body, "typescript", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "computed-call");
    assert_eq!(m[0].key, None);
}

#[test]
fn does_not_fire_on_dispatch_shapes_inside_comments_or_strings() {
    let body = [
        "function safe() {",
        "  // this.handlers[action.type](payload) — commented out",
        "  const doc = \"call handlers[key](p) to dispatch\";",
        "  return 1;",
        "}",
    ]
    .join("\n");
    assert!(scan_dynamic_dispatch(&body, "typescript", 1).is_empty());
}

#[test]
fn does_not_treat_plain_indexing_or_array_literals_as_dispatch() {
    let body = "function f(xs) {\n  const a = xs[0];\n  const b = [1, 2, 3];\n  return a + b[1];\n}";
    assert!(scan_dynamic_dispatch(body, "typescript", 1).is_empty());
}

#[test]
fn detects_python_getattr_immediate_call() {
    let body = "def run(self, name):\n    return getattr(self, name)(1)";
    let m = scan_dynamic_dispatch(body, "python", 5);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "getattr-call");
}

#[test]
fn detects_two_step_getattr_only_when_the_assigned_name_is_called_later() {
    let called = "def process(self, kind, p):\n    handler = getattr(self, 'handle_' + kind)\n    return handler(p)";
    let m = scan_dynamic_dispatch(called, "python", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "getattr-assign");
    assert_eq!(m[0].key.as_deref(), Some("handle_")); // the literal prefix

    let not_called = "def peek(self, kind):\n    handler = getattr(self, 'handle_' + kind)\n    return handler";
    assert!(scan_dynamic_dispatch(not_called, "python", 1).is_empty());
}

#[test]
fn detects_ruby_send_with_a_symbol_key() {
    let body = "def run(name)\n  target.send(:handle_save, 1)\nend";
    let m = scan_dynamic_dispatch(body, "ruby", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "ruby-send");
    assert_eq!(m[0].key.as_deref(), Some("handle_save"));
}

#[test]
fn detects_typed_message_dispatch_and_marks_the_key_as_a_type() {
    let body = "public async Task<int> Create(CreateCmd c) {\n  return await _mediator.Send(new CreateTodoItemCommand(c));\n}";
    let m = scan_dynamic_dispatch(body, "csharp", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "typed-bus");
    assert_eq!(m[0].key.as_deref(), Some("CreateTodoItemCommand"));
    assert!(m[0].key_is_type);
}

#[test]
fn detects_runtime_keyed_emit_but_not_literal_keyed_emit() {
    let runtime = "notify(name, data) {\n  this.emitter.emit(name, data);\n}";
    let m = scan_dynamic_dispatch(runtime, "typescript", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].form, "var-key-dispatch");

    // Literal keys are the edge synthesizer's territory — not a boundary.
    let literal = "notify(data) {\n  this.emitter.emit('saved', data);\n}";
    assert!(scan_dynamic_dispatch(literal, "typescript", 1).is_empty());
}

#[test]
fn dedupes_repeated_same_form_same_key_sites_and_counts_the_extras() {
    let body = [
        "route(a) {",
        "  this.table[a.type](a.p);",
        "  this.table[a.kind](a.p);",
        "  this.table[a.name](a.p);",
        "}",
    ]
    .join("\n");
    let m = scan_dynamic_dispatch(&body, "typescript", 1);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].more_sites, 2);
}

#[test]
fn detects_reflective_dispatch_with_a_literal_method_name_as_key() {
    let body = "public void run(Object o) {\n  o.getClass().getMethod(\"handlePing\").invoke(o);\n}";
    let m = scan_dynamic_dispatch(body, "java", 1);
    assert!(!m.is_empty());
    assert_eq!(m[0].form, "reflection");
    assert_eq!(m[0].key.as_deref(), Some("handlePing"));
}

#[test]
fn flags_an_import_built_from_a_template_literal_with_a_substitution() {
    assert_eq!(
        forms("async function load(lang) {\n  return import(`./locales/${lang}.js`);\n}"),
        ["dynamic-import"]
    );
    assert_eq!(
        forms("function load(name) {\n  return require(`./plugins/${name}`);\n}"),
        ["dynamic-import"]
    );
}

#[test]
fn flags_an_import_built_by_string_concatenation() {
    assert_eq!(
        forms("async function load(lang) {\n  return import('./locales/' + lang + '.js');\n}"),
        ["dynamic-import"]
    );
    assert_eq!(
        forms("function load(dir) {\n  return require(dir + \"/index\");\n}"),
        ["dynamic-import"]
    );
}

#[test]
fn still_flags_an_import_of_a_bare_expression() {
    assert_eq!(
        forms("async function load(p) {\n  return import(p);\n}"),
        ["dynamic-import"]
    );
}

#[test]
fn leaves_a_single_complete_literal_alone() {
    assert!(forms("async function a() {\n  return import('./fixed.js');\n}").is_empty());
    assert!(forms("async function a() {\n  return import(`./fixed.js`);\n}").is_empty());
    assert!(forms("function a() {\n  return require(\"./fixed\");\n}").is_empty());
    assert!(forms(
        "async function a() {\n  return import('./data.json', { with: { type: 'json' } });\n}"
    )
    .is_empty());
}

#[test]
fn import_escapes_and_trivia_are_respected() {
    let static_args = [
        "`./plugins/\\${name}`",
        "`./plugins/\\\\\\${name}`",
        "`./plugins/\\`fixed`",
        "\"./plugins/\\\"fixed\"",
        "'./plugins/\\'fixed'",
        "/* before */ `./fixed` /* after */",
        "// before\n \"./fixed\" // after\n",
        "`./fixed`, { with: { type: \"json\" } }",
        "\"./${name}\"",
    ];
    for arg in static_args {
        for call in ["import", "require"] {
            assert!(
                scan_dynamic_dispatch(&format!("{call}({arg})"), "typescript", 1).is_empty(),
                "{call}({arg})"
            );
        }
    }
    let runtime_args = [
        "`./plugins/\\\\${name}`",
        "`./plugins/\\${literal}/${name}`",
        "`./plugins/${`nested-${name}`}`",
        "/* before */ \"./plugins/\" /* after */ + name",
        "`./plugins/\\${literal}` + name",
        "\"./plugins/\".concat(name)",
        "// before\n `./plugins/${name}`, { with: { type: \"json\" } }",
    ];
    for arg in runtime_args {
        for call in ["import", "require"] {
            let body = format!("function load() {{\n  {call}({arg});\n}}");
            let matches = scan_dynamic_dispatch(&body, "typescript", 20);
            assert_eq!(matches.len(), 1, "{call}({arg})");
            assert_eq!(matches[0].form, "dynamic-import", "{call}({arg})");
            assert_eq!(matches[0].line, 21, "{call}({arg})");
        }
    }
}

#[test]
fn detects_runtime_imports_across_the_js_family() {
    for language in [
        "javascript",
        "typescript",
        "jsx",
        "tsx",
        "vue",
        "svelte",
        "astro",
        "arkts",
    ] {
        let first = scan_dynamic_dispatch("import(`./${name}`)", language, 1);
        assert_eq!(first.first().map(|m| m.form.as_str()), Some("dynamic-import"));
        assert!(
            scan_dynamic_dispatch("import(`./fixed`)", language, 1).is_empty(),
            "{language}"
        );
    }
}
