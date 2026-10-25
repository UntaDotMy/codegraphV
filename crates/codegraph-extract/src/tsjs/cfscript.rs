//! The cfscript and cfquery paths of `TreeSitterExtractor` (src/extraction/tree-sitter.ts) over
//! the configs of languages/cfscript.ts and languages/cfquery.ts.
//!
//! cfscript's grammar is JavaScript's with CFML on top (`component`, `property`, `include`,
//! access modifiers), and the TypeScript engine walks it with the generic walker: every TS/JS
//! path that is not gated by language runs for cfscript too (a declarator's function value, the
//! store and component-wrapper shapes of a variable). So it is a variant of this walker, with its
//! own dispatch ladder and body walk — the config differs (`component` classes, function
//! declarations as methods inside one, `property` declarations, `include` imports) and the TS/JS
//! hooks (exports, async, static, type annotations, value and function-as-value refs) are absent.
//! cfquery parses the `#…#` expressions in a `<cfquery>` body; its config maps only calls.

use super::{Extra, Variant, Walker};
use crate::buffers::edge_kind_index;
use crate::ts::Node;
use codegraph_types::jscompat::{is_js_space, js_trim};

impl<'t> Walker<'t> {
    /// dispatchNode for the cfscript / cfquery configs.
    pub(super) fn visit_node_cf(&mut self, node: Node<'t>) {
        stack_guard!();
        let kind = node.kind();
        if self.variant == Variant::Cfquery {
            // functionTypes … importTypes are all empty: a call is a call, `new` an instantiation,
            // and every node's children are walked.
            if kind == "call_expression" {
                self.extract_cf_call(node);
            } else if kind == "new_expression" {
                self.extract_instantiation(node);
            }
            self.visit_children_cf(node);
            return;
        }
        let mut skip_children = true;
        if matches!(kind, "function_declaration" | "function_expression" | "arrow_function") {
            if self.inside_class_like() && kind == "function_declaration" {
                self.extract_method(node);
            } else {
                self.extract_function(node, None);
            }
        } else if kind == "component" {
            self.extract_component(node);
        } else if kind == "method_definition" {
            self.extract_method(node);
        } else if kind == "property_declaration" && self.inside_class_like() {
            self.extract_property(node);
        } else if kind == "variable_declaration" && !self.inside_class_like() {
            self.extract_variable(node);
        } else {
            skip_children = false;
            if matches!(kind, "import_statement" | "include_statement") {
                self.extract_cf_import(node);
            } else if kind == "call_expression" {
                self.extract_cf_call(node);
            } else if kind == "new_expression" {
                self.extract_instantiation(node);
            }
        }
        if !skip_children {
            self.visit_children_cf(node);
        }
    }

    fn visit_children_cf(&mut self, node: Node<'t>) {
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i) {
                self.visit_node_cf(c);
            }
        }
    }

    /// classifyClassNode: `interface { … }` is the same node as `component { … }`, told apart by
    /// its first token.
    fn extract_component(&mut self, node: Node<'t>) {
        if node.child(0).is_some_and(|c| c.kind() == "interface") {
            self.extract_interface(node);
        } else {
            self.extract_class(node);
        }
    }

    /// visitFunctionBody for cfscript.
    pub(super) fn visit_body_cf(&mut self, node: Node<'t>) {
        stack_guard!();
        let kind = node.kind();
        // A CFML `include "x.cfm";` in a function body includes the template there (#2651).
        if kind == "include_statement" {
            self.extract_cf_import(node);
        }
        if kind == "call_expression" {
            self.extract_cf_call(node);
        } else if kind == "new_expression" {
            self.extract_instantiation(node);
        }
        // Nested NAMED functions become their own nodes; an anonymous one is part of its scope.
        if matches!(kind, "function_declaration" | "function_expression" | "arrow_function") && self.extract_name(node) != "<anonymous>" {
            self.extract_function(node, None);
            return;
        }
        if kind == "component" {
            self.extract_component(node);
            return;
        }
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i) {
                self.visit_body_cf(c);
            }
        }
    }

    /// cfmlVisibility: an `access_type` child's keyword.
    pub(super) fn cf_visibility(&self, node: Node) -> Option<u8> {
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else { continue };
            if child.kind() == "access_type" {
                return match self.text(child) {
                    "public" | "remote" => Some(1),
                    "private" => Some(2),
                    "package" => Some(4),
                    _ => None,
                };
            }
        }
        None
    }

    /// extractImport over cfscriptExtractor.extractImport: `include "x.cfm";` / `include
    /// template="x.cfm";`, `cfinclude(…)`, and `import a.b.C;` / `import "java:…";`. A declined
    /// statement makes nothing.
    pub(super) fn extract_cf_import(&mut self, node: Node<'t>) {
        let import_text = js_trim(self.text(node)).to_string();
        let module_name = match node.kind() {
            "include_statement" => {
                let named: Vec<Node> = (0..node.named_child_count()).filter_map(|i| node.named_child(i)).collect();
                match named.iter().find(|c| c.kind() == "string") {
                    Some(expr) => self.literal_string(Some(*expr)),
                    None => named.iter().find(|c| c.kind() == "assignment_expression").and_then(|a| self.template_argument(*a)),
                }
            }
            "call_expression" => {
                if self.is_cf_include_call(node) {
                    self.cf_include_call_template(node)
                } else {
                    None
                }
            }
            _ => node.child_by_field_name("source").map(|source| {
                if source.kind() == "import_path" {
                    (0..source.named_child_count())
                        .filter_map(|i| source.named_child(i))
                        .map(|c| self.text(c))
                        .collect::<Vec<_>>()
                        .join(".")
                } else {
                    strip_one_quote_each_end(self.text(source)).to_string()
                }
            }),
        };
        let Some(module_name) = module_name.filter(|m| !m.is_empty()) else { return };
        self.create_node("import", &module_name, node, Extra { signature: Some(import_text), ..Extra::default() });
        let parent = self.top_row();
        self.push_ref(parent, &module_name, edge_kind_index("imports").unwrap(), node);
    }

    /// extractCall for cfscript and cfquery: the generic (non-TS/JS) callee naming, with
    /// cfscript's `cfinclude`, `createObject` and component-receiver shapes.
    pub(super) fn extract_cf_call(&mut self, node: Node<'t>) {
        if self.stack.is_empty() {
            return;
        }
        let cfscript = self.variant == Variant::Cfscript;
        if cfscript && self.is_cf_include_call(node) {
            // An include of a template, never a call (#2651).
            self.extract_cf_import(node);
            return;
        }
        if cfscript {
            if let Some(created) = self.cfml_created_component(node) {
                // An instance of the component, under the path as written (#2652).
                let from = self.top_row();
                self.push_ref(from, &created, edge_kind_index("instantiates").unwrap(), node);
                return;
            }
        }
        let Some(func) = node.child_by_field_name("function").or_else(|| node.named_child(0)) else { return };
        let mut callee_name = String::new();
        if matches!(func.kind(), "member_expression" | "attribute" | "selector_expression" | "navigation_expression" | "field_expression") {
            let property = func
                .child_by_field_name("property")
                .or_else(|| func.child_by_field_name("field"))
                .or_else(|| func.named_child(1));
            if let Some(property) = property {
                let method_name = self.text(property);
                let receiver = func
                    .child_by_field_name("object")
                    .or_else(|| func.child_by_field_name("operand"))
                    .or_else(|| func.child_by_field_name("argument"))
                    .or_else(|| func.named_child(0));
                if receiver.is_some_and(|r| super::is_literal_receiver(r.kind())) {
                    return;
                }
                callee_name = match receiver {
                    Some(r) if matches!(r.kind(), "identifier" | "simple_identifier" | "field_identifier") => {
                        let receiver_name = self.text(r);
                        if matches!(receiver_name, "self" | "this" | "cls" | "super") {
                            method_name.to_string()
                        } else {
                            format!("{receiver_name}.{method_name}")
                        }
                    }
                    Some(r) if cfscript && self.cfml_constructed_component(r).is_some() => {
                        format!("{}().{method_name}", self.cfml_constructed_component(r).unwrap_or_default())
                    }
                    // `variables.svc.save()` / `arguments.svc.save()`: a field reached through a
                    // CFML scope keeps its receiver chain (#1108).
                    Some(r) if cfscript && r.kind() == "member_expression" && is_cf_scope_member(self.text(r)) => {
                        format!("{}.{method_name}", self.text(r))
                    }
                    _ => method_name.to_string(),
                };
            }
        } else {
            callee_name = self.text(func).to_string();
        }
        if let Some(c) = crate::textutil::paren_conversion().captures(&callee_name) {
            callee_name = c[1].to_string();
        }
        if !callee_name.is_empty() {
            self.push_call_ref(&callee_name, node);
        }
    }

    /// The component inheritance a cfscript `component extends="Base" implements="IFoo,IBar"`
    /// (or `interface extends="IBase"`) attribute names.
    pub(super) fn cf_component_attribute_inheritance(&mut self, attr: Node<'t>, class_row: u32) {
        let named: Vec<Node> = (0..attr.named_child_count()).filter_map(|i| attr.named_child(i)).collect();
        let (Some(label), Some(value)) =
            (named.iter().find(|c| c.kind() == "identifier"), named.iter().find(|c| c.kind() != "identifier"))
        else {
            return;
        };
        let label = self.text(*label).to_lowercase();
        if label != "extends" && label != "implements" {
            return;
        }
        let value_text = strip_one_quote_each_end(self.text(*value));
        let names: Vec<&str> = if label == "implements" {
            value_text.split(',').map(js_trim).filter(|s| !s.is_empty()).collect()
        } else {
            Some(js_trim(value_text)).filter(|s| !s.is_empty()).into_iter().collect()
        };
        let kind = edge_kind_index(if label == "implements" { "implements" } else { "extends" }).unwrap();
        for name in names {
            let name = name.to_string();
            self.push_ref(class_row, &name, kind, *value);
        }
    }

    /// literalString: a string literal written out in full, or None when a `#…#` expression
    /// builds it at runtime.
    fn literal_string(&self, node: Option<Node>) -> Option<String> {
        let node = node.filter(|n| n.kind() == "string")?;
        if (0..node.named_child_count()).filter_map(|i| node.named_child(i)).any(|c| c.kind() != "string_fragment") {
            return None;
        }
        Some(js_trim(strip_one_quote_each_end(self.text(node))).to_string()).filter(|s| !s.is_empty())
    }

    /// templateArgument: the literal value of a `template=…` argument.
    fn template_argument(&self, node: Node) -> Option<String> {
        if node.kind() != "assignment_expression" {
            return None;
        }
        let left = node.child_by_field_name("left").filter(|l| l.kind() == "identifier")?;
        if self.text(left).to_lowercase() != "template" {
            return None;
        }
        self.literal_string(node.child_by_field_name("right"))
    }

    /// isCfIncludeCall: `cfinclude(…)`.
    fn is_cf_include_call(&self, node: Node) -> bool {
        node.child_by_field_name("function")
            .is_some_and(|f| f.kind() == "identifier" && self.text(f).to_lowercase() == "cfinclude")
    }

    /// cfIncludeCallTemplate: `cfinclude(template="x.cfm")` or `cfinclude("x.cfm")`.
    fn cf_include_call_template(&self, node: Node) -> Option<String> {
        let args = node.child_by_field_name("arguments")?;
        let named: Vec<Node> = (0..args.named_child_count()).filter_map(|i| args.named_child(i)).collect();
        for arg in &named {
            if let Some(named) = self.template_argument(*arg) {
                return Some(named);
            }
        }
        self.literal_string(named.first().copied())
    }

    /// cfmlCreatedComponent: the component `createObject("component", "a.b.Batch")`,
    /// `createObject("a.b.Batch")` or `createObject(type="component", component="a.b.Batch")`
    /// makes, as written (#2652).
    fn cfml_created_component(&self, call: Node) -> Option<String> {
        if call.kind() != "call_expression" {
            return None;
        }
        let f = call.child_by_field_name("function")?;
        if f.kind() != "identifier" || self.text(f).to_lowercase() != "createobject" {
            return None;
        }
        let args: Vec<Node> = call
            .child_by_field_name("arguments")
            .map(|a| (0..a.named_child_count()).filter_map(|i| a.named_child(i)).collect())
            .unwrap_or_default();
        // Outer None: undefined (never set); inner None: set to a non-literal.
        let mut type_: Option<Option<String>> = None;
        let mut name: Option<String> = None;
        if !args.is_empty() && args.iter().all(|a| a.kind() == "assignment_expression") {
            for arg in &args {
                let key = self.text(arg.child_by_field_name("left").unwrap_or(*arg)).to_lowercase();
                if key == "type" {
                    type_ = Some(self.literal_string(arg.child_by_field_name("right")));
                } else if key == "component" {
                    name = self.literal_string(arg.child_by_field_name("right"));
                }
            }
        } else if args.len() == 1 {
            name = self.literal_string(Some(args[0]));
        } else if args.len() >= 2 {
            type_ = Some(self.literal_string(Some(args[0])));
            name = self.literal_string(Some(args[1]));
        }
        if let Some(t) = type_ {
            if t.map(|t| t.to_lowercase()).as_deref() != Some("component") {
                return None;
            }
        }
        name.filter(|n| is_component_path(n))
    }

    /// cfmlConstructedComponent: the component `new a.b.Batch()` or a component
    /// `createObject(…)` constructs, parenthesized or not (#2652).
    fn cfml_constructed_component(&self, node: Node) -> Option<String> {
        let mut expr = Some(node);
        while let Some(e) = expr.filter(|e| e.kind() == "parenthesized_expression") {
            expr = e.named_child(0);
        }
        let expr = expr?;
        if expr.kind() == "new_expression" {
            let name: String = expr
                .child_by_field_name("constructor")
                .map(|c| self.text(c).chars().filter(|ch| !is_js_space(*ch)).collect())
                .unwrap_or_default();
            return is_component_path(&name).then_some(name);
        }
        self.cfml_created_component(expr)
    }
}

/// `.replace(/^["']|["']$/g, '')`: one quote off each end.
fn strip_one_quote_each_end(s: &str) -> &str {
    let s = s.strip_prefix(['"', '\'']).unwrap_or(s);
    s.strip_suffix(['"', '\'']).unwrap_or(s)
}

/// COMPONENT_PATH: `/^[A-Za-z_][\w$-]*(?:\.[A-Za-z_][\w$-]*)*$/` — `Batch`, `a.b.Batch`.
fn is_component_path(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|seg| {
            let mut chars = seg.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-'))
        })
}

/// `/^(variables|this|local|arguments)\.[A-Za-z_][\w]*$/i`.
fn is_cf_scope_member(text: &str) -> bool {
    let Some((scope, rest)) = text.split_once('.') else { return false };
    if !["variables", "this", "local", "arguments"].iter().any(|s| s.eq_ignore_ascii_case(scope)) {
        return false;
    }
    let mut chars = rest.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_paths_and_scope_members() {
        assert!(is_component_path("a.b-c.Batch$"));
        assert!(!is_component_path("a..b"));
        assert!(!is_component_path("1a"));
        assert!(!is_component_path(""));
        assert!(is_cf_scope_member("Variables.svc"));
        assert!(!is_cf_scope_member("variables.svc.x"));
        assert!(!is_cf_scope_member("request.svc"));
        assert_eq!(strip_one_quote_each_end("\"x'"), "x");
        assert_eq!(strip_one_quote_each_end("x"), "x");
    }
}

