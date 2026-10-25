//! Objective-C on the C walker: the objc extractor's hooks (languages/objc.ts)
//! and the objc-only branches of tree-sitter.ts — `@interface` classes and
//! their supertypes, `@protocol`s, `@implementation` (which reuses the class
//! node), selector-named methods, `@property`, and `[receiver message:]`
//! calls. Everything else takes the C paths: Objective-C is a C superset, and
//! tree-sitter.ts walks it with the C branches its language gates name.

use super::{Extra, Scope, Variant, Walker};
use crate::buffers::edge_kind_index;
use crate::docstring::preceding_docstring;
use crate::textutil as util;
use crate::ts::Node;

/// OBJC_TYPE_QUALIFIERS: nullability / ARC qualifiers that sit where a return
/// type's first type identifier does — never the type.
const TYPE_QUALIFIERS: &[&str] = &[
    "nonnull", "nullable", "null_unspecified", "null_resettable", "_Nonnull", "_Nullable",
    "_Null_unspecified", "__nonnull", "__nullable", "const", "volatile", "strong", "weak", "copy",
    "assign", "retain", "oneway", "__strong", "__weak", "__unsafe_unretained", "__autoreleasing",
    "__kindof",
];

/// JS `\s` (WhiteSpace ∪ LineTerminator).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{00A0}' | '\u{1680}'
            | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' | '\u{FEFF}'
    )
}

fn is_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `/^[A-Za-z_]\w*$/`.
fn is_simple_ident(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_') && is_word(s)
}

/// `/^[A-Z][A-Za-z0-9_]*$/`: a class-message receiver.
fn is_class_name(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_uppercase) && is_word(s)
}

fn named_children<'n>(node: Node<'n>) -> impl Iterator<Item = Node<'n>> {
    (0..node.named_child_count()).filter_map(move |i| node.named_child(i))
}

impl<'t> Walker<'t> {

    /// The `method` keywords of a message, in order, and whether it has a `:`
    /// (a single keyword may be unary, `[c reset]`, or take one argument).
    fn message_selector(&self, node: Node<'t>) -> Option<String> {
        let mut cursor = node.walk();
        let keywords: Vec<&str> = node.children_by_field_name("method", &mut cursor).map(|k| self.text(k)).collect();
        let first = *keywords.first()?;
        let has_colon = (0..node.child_count()).any(|i| node.child(i).is_some_and(|c| c.kind() == ":"));
        Some(if has_colon { keywords.iter().map(|k| format!("{k}:")).collect() } else { first.to_string() })
    }

    /// extractObjcMethodName: a method is named by its selector — `greet`,
    /// `doThing:`, `doThing:with:`.
    pub(super) fn objc_method_name(&self, node: Node<'t>) -> Option<String> {
        if !matches!(node.kind(), "method_definition" | "method_declaration") {
            return None;
        }
        let mut identifiers = Vec::new();
        let mut has_parameters = false;
        for child in named_children(node) {
            match child.kind() {
                "identifier" => identifiers.push(self.text(child)),
                "method_parameter" => has_parameters = true,
                _ => {}
            }
        }
        let first = *identifiers.first()?;
        let name = if has_parameters { identifiers.iter().map(|id| format!("{id}:")).collect() } else { first.to_string() };
        (!name.is_empty()).then_some(name)
    }

    /// extractObjcReturnType: the declared return type as a bare class name
    /// (`+ (Bar *)create` → `Bar`), past nullability/ARC qualifiers; none for
    /// `void` / `id` / `instancetype` / anything that is not an identifier.
    pub(super) fn objc_return_type(&self, node: Node<'_>) -> Option<String> {
        if !matches!(node.kind(), "method_definition" | "method_declaration") {
            return None;
        }
        let method_type = named_children(node).find(|c| c.kind() == "method_type")?;
        let mut ids = Vec::new();
        self.collect_type_identifiers(method_type, &mut ids);
        let name = ids.into_iter().find(|n| !TYPE_QUALIFIERS.contains(&n.as_str()))?;
        (is_simple_ident(&name) && !matches!(name.as_str(), "void" | "id" | "instancetype")).then_some(name)
    }

    fn collect_type_identifiers(&self, node: Node<'_>, out: &mut Vec<String>) {
        stack_guard!();
        if node.kind() == "type_identifier" {
            out.push(self.text(node).trim_matches(is_js_space).to_string());
        }
        for child in named_children(node) {
            self.collect_type_identifiers(child, out);
        }
    }

    /// objcExtractor.isStatic (`/^\s*\+/` on the node's text); None for C/C++,
    /// which have no isStatic hook.
    pub(super) fn objc_is_static(&self, node: Node<'t>) -> Option<bool> {
        (self.variant == Variant::Objc).then(|| self.text(node).trim_start_matches(is_js_space).starts_with('+'))
    }

    /// objcExtractor.resolveBody: the `body` field, else the first
    /// `compound_statement` child (a method's body has no field). C and C++
    /// read the field alone.
    pub(super) fn body_of(&self, node: Node<'t>) -> Option<Node<'t>> {
        node.child_by_field_name("body").or_else(|| {
            if self.variant != Variant::Objc {
                return None;
            }
            named_children(node).find(|c| c.kind() == "compound_statement")
        })
    }

    /// extractClass for an `@interface` (classTypes; no body field, so the
    /// whole node is walked for its members).
    pub(super) fn extract_objc_class(&mut self, node: Node<'t>) {
        stack_guard!();
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.class_like_docstring(node), ..Extra::default() };
        let Some(row) = self.create_node("class", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.stack.push(Scope { row, kind: "class", name, qualified: None });
        let body = self.body_of(node).unwrap_or(node);
        for child in named_children(body) {
            self.visit_node(child);
        }
        self.stack.pop();
    }

    /// extractInterface for a `@protocol` (interfaceKind 'protocol').
    pub(super) fn extract_objc_protocol(&mut self, node: Node<'t>) {
        stack_guard!();
        let name = self.extract_name(node);
        let extra = Extra { docstring: self.class_like_docstring(node), ..Extra::default() };
        let Some(row) = self.create_node("protocol", &name, node, extra) else { return };
        self.extract_inheritance(node, row);
        self.stack.push(Scope { row, kind: "protocol", name, qualified: None });
        let body = self.body_of(node).unwrap_or(node);
        for child in named_children(body) {
            self.visit_node(child);
        }
        self.stack.pop();
    }

    /// The objc visitNode hook: an `@implementation` reuses the file's class
    /// node of its name (or mints one) and walks its definitions under it.
    pub(super) fn extract_objc_implementation(&mut self, node: Node<'t>) {
        stack_guard!();
        let Some(name_node) = named_children(node).find(|c| c.kind() == "identifier") else { return };
        let class_name = self.text(name_node).to_string();
        let existing = self.nodes_meta.iter().position(|m| m.kind == "class" && m.name == class_name).map(|i| i as u32);
        let Some(row) = existing.or_else(|| self.create_node("class", &class_name, node, Extra::default())) else { return };
        self.stack.push(Scope { row, kind: "class", name: class_name, qualified: None });
        for child in named_children(node) {
            if child.kind() == "implementation_definition" {
                for impl_child in named_children(child) {
                    self.visit_node(impl_child);
                }
            }
        }
        self.stack.pop();
    }

    /// extractObjcPropertyName: the identifier under the property's
    /// struct_declarator, through pointer declarators.
    fn objc_property_name(&self, node: Node<'t>) -> Option<String> {
        let struct_decl = named_children(node).find(|c| c.kind() == "struct_declaration")?;
        let mut current = named_children(struct_decl).find(|c| c.kind() == "struct_declarator")?;
        loop {
            let inner = current
                .child_by_field_name("declarator")
                .or_else(|| named_children(current).find(|c| matches!(c.kind(), "identifier" | "pointer_declarator")))?;
            if inner.kind() == "identifier" {
                return Some(self.text(inner).to_string());
            }
            current = inner;
        }
    }

    /// extractProperty for an `@property` inside a class.
    pub(super) fn extract_objc_property(&mut self, node: Node<'t>) {
        let name = match self.objc_property_name(node) {
            Some(name) => name,
            None => match named_children(node).find(|c| c.kind() == "identifier") {
                Some(id) => self.text(id).to_string(),
                None => return,
            },
        };
        if name.is_empty() {
            return;
        }
        let type_node = named_children(node).find(|c| {
            !matches!(c.kind(), "modifier" | "modifiers" | "identifier" | "accessor_list" | "accessors" | "equals_value_clause")
        });
        let type_text = type_node.map(|t| {
            let text = self.text(t);
            match text.strip_prefix(':') {
                Some(rest) => rest.trim_start_matches(is_js_space),
                None => text,
            }
        });
        let signature = match type_text {
            Some(t) if !t.is_empty() => format!("{t} {name}"),
            _ => name.clone(),
        };
        let extra = Extra {
            docstring: preceding_docstring(node, self.src),
            signature: Some(signature),
            is_static: self.objc_is_static(node),
            ..Extra::default()
        };
        self.create_node("property", &name, node, extra);
    }

    /// extractInheritance for an `@interface`: the superclass `extends`, each
    /// protocol in `<…>` `implements`.
    pub(super) fn extract_objc_class_inheritance(&mut self, node: Node<'t>, class_row: u32) {
        if let Some(superclass) = node.child_by_field_name("superclass") {
            let name = self.text(superclass).to_string();
            self.push_ref_at(class_row, &name, edge_kind_index("extends").unwrap(), superclass);
        }
        let implements = edge_kind_index("implements").unwrap();
        let lists: Vec<Node<'t>> = named_children(node).filter(|c| c.kind() == "parameterized_arguments").collect();
        for list in lists {
            let type_ids: Vec<Node<'t>> = named_children(list)
                .filter_map(|t| named_children(t).find(|c| matches!(c.kind(), "type_identifier" | "identifier")))
                .collect();
            for type_id in type_ids {
                let name = self.text(type_id).to_string();
                self.push_ref_at(class_row, &name, implements, type_id);
            }
        }
    }

    /// The message_expression branch of extractCall: `[recv a:1 b:2]` calls
    /// `recv.a:b:`; a capitalized receiver is also a `references` to the
    /// class; a class-factory chain `[[Foo create] doIt]` is
    /// `Foo.create().doIt`; `self` / `super` receivers drop.
    pub(super) fn extract_message_call(&mut self, node: Node<'t>) {
        let caller = self.top_row();
        let Some(method_name) = self.message_selector(node) else { return };
        let receiver = node.child_by_field_name("receiver");
        let mut callee = match receiver {
            Some(r) if r.kind() != "message_expression" => {
                let receiver_name = self.text(r);
                if !receiver_name.is_empty() && !matches!(receiver_name, "self" | "super") {
                    if is_class_name(receiver_name) {
                        self.push_ref_at(caller, receiver_name, edge_kind_index("references").unwrap(), r);
                    }
                    format!("{receiver_name}.{method_name}")
                } else {
                    method_name
                }
            }
            Some(r) if is_word(&method_name) => {
                let inner = r.child_by_field_name("receiver");
                let inner_name = inner.map(|i| self.text(i)).unwrap_or("");
                let inner_selector = (inner.is_some_and(|i| i.kind() == "identifier")
                    && inner_name.as_bytes().first().is_some_and(u8::is_ascii_uppercase))
                .then(|| self.message_selector(r))
                .flatten();
                match inner_selector {
                    Some(selector) if !selector.is_empty() => format!("{inner_name}.{selector}().{method_name}"),
                    _ => method_name,
                }
            }
            _ => method_name,
        };
        if let Some(c) = util::paren_conversion().captures(&callee) {
            callee = c[1].to_string();
        }
        if !callee.is_empty() {
            self.push_ref_at(caller, &callee, edge_kind_index("calls").unwrap(), node);
        }
    }

    /// normalizeSpecial's `selector_expression` for ObjC `@selector(…)`: the
    /// inner identifier, or the selector text as written.
    pub(super) fn objc_selector_ref(&self, node: Node<'t>) -> Option<(String, Node<'t>)> {
        let inner = node.named_child(0)?;
        if matches!(inner.kind(), "identifier" | "simple_identifier") {
            return Some((self.text(inner).to_string(), inner));
        }
        Some((self.text(inner).trim_matches(is_js_space).to_string(), inner))
    }
}
