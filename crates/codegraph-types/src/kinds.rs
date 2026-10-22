//! String-valued enums of the graph vocabulary.
//!
//! The variant ORDER of `NodeKind` and `EdgeKind` is the order of `NODE_KINDS`
//! / `EDGE_KINDS` in `src/types.ts`, which is also the index order of the
//! native kernel's wire contract. Append new kinds, never reorder.

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Defines a fieldless enum whose canonical form is a fixed string, with the
/// conversions every layer needs (`as_str`, `FromStr`, `Display`, serde, the
/// `ALL` table). One definition per enum keeps the string table and the
/// variant list from drifting.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The canonical string (what the database and JSON carry).
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            /// Parse the canonical string; `None` for anything else.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }

            /// Position in `ALL` (the wire index for kinds).
            pub const fn index(self) -> u8 {
                self as u8
            }
        }

        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s).ok_or_else(|| format!(concat!("unknown ", stringify!($name), ": {}"), s))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(&s).ok_or_else(|| {
                    de::Error::custom(format!(concat!("unknown ", stringify!($name), ": {}"), s))
                })
            }
        }
    };
}

string_enum! {
    /// Types of nodes in the knowledge graph.
    pub enum NodeKind {
        File => "file",
        Module => "module",
        Class => "class",
        Struct => "struct",
        Interface => "interface",
        Trait => "trait",
        Protocol => "protocol",
        Function => "function",
        Method => "method",
        Property => "property",
        Field => "field",
        Variable => "variable",
        Constant => "constant",
        Enum => "enum",
        EnumMember => "enum_member",
        TypeAlias => "type_alias",
        Namespace => "namespace",
        Parameter => "parameter",
        Import => "import",
        Export => "export",
        Route => "route",
        Component => "component",
        Union => "union",
    }
}

string_enum! {
    /// Types of edges (relationships) between nodes.
    pub enum EdgeKind {
        /// Parent contains child (file→class, class→method)
        Contains => "contains",
        /// Function/method calls another
        Calls => "calls",
        /// File imports from another
        Imports => "imports",
        /// File exports a symbol
        Exports => "exports",
        /// Class/interface extends another
        Extends => "extends",
        /// Class implements interface
        Implements => "implements",
        /// Generic reference to another symbol
        References => "references",
        /// Variable/parameter has type
        TypeOf => "type_of",
        /// Function returns type
        Returns => "returns",
        /// Creates instance of class
        Instantiates => "instantiates",
        /// Method overrides parent method
        Overrides => "overrides",
        /// Decorator applied to symbol
        Decorates => "decorates",
        /// Navigates to a screen/route (Expo Router `router.push('/x')`)
        Navigates => "navigates",
    }
}

string_enum! {
    /// Supported programming languages (and file-level formats).
    pub enum Language {
        TypeScript => "typescript",
        JavaScript => "javascript",
        Tsx => "tsx",
        Jsx => "jsx",
        ArkTs => "arkts",
        Python => "python",
        Go => "go",
        Rust => "rust",
        Java => "java",
        C => "c",
        Cpp => "cpp",
        CSharp => "csharp",
        Razor => "razor",
        Php => "php",
        Ruby => "ruby",
        Swift => "swift",
        Kotlin => "kotlin",
        Dart => "dart",
        Svelte => "svelte",
        Vue => "vue",
        Astro => "astro",
        Liquid => "liquid",
        Pascal => "pascal",
        Scala => "scala",
        Lua => "lua",
        Luau => "luau",
        ObjC => "objc",
        R => "r",
        Solidity => "solidity",
        Nix => "nix",
        Yaml => "yaml",
        Twig => "twig",
        Xml => "xml",
        Properties => "properties",
        Cfml => "cfml",
        CfScript => "cfscript",
        CfQuery => "cfquery",
        Cobol => "cobol",
        VbNet => "vbnet",
        Erlang => "erlang",
        Terraform => "terraform",
        Unknown => "unknown",
    }
}

string_enum! {
    /// Visibility modifier of a symbol.
    pub enum Visibility {
        Public => "public",
        Private => "private",
        Protected => "protected",
        Internal => "internal",
    }
}

string_enum! {
    /// How an edge was created.
    pub enum Provenance {
        TreeSitter => "tree-sitter",
        Scip => "scip",
        Heuristic => "heuristic",
    }
}

string_enum! {
    /// Severity of an extraction error.
    pub enum Severity {
        Error => "error",
        Warning => "warning",
    }
}

string_enum! {
    /// Direction of a graph traversal.
    pub enum Direction {
        Outgoing => "outgoing",
        Incoming => "incoming",
        Both => "both",
    }
}

string_enum! {
    /// Retrieval confidence of a context-style query.
    pub enum Confidence {
        High => "high",
        Low => "low",
    }
}

string_enum! {
    /// Output format of a built context.
    pub enum ContextFormat {
        Markdown => "markdown",
        Json => "json",
    }
}

/// Kinds an unresolved reference can carry: every edge kind, plus the
/// internal-only `function_ref` — a function name used as a VALUE (callback
/// registration, #756). It never becomes an edge kind; resolution maps it to a
/// `references` edge targeting function/method nodes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReferenceKind {
    Edge(EdgeKind),
    FunctionRef,
}

impl ReferenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReferenceKind::Edge(k) => k.as_str(),
            ReferenceKind::FunctionRef => "function_ref",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        if s == "function_ref" {
            Some(ReferenceKind::FunctionRef)
        } else {
            EdgeKind::parse(s).map(ReferenceKind::Edge)
        }
    }

    /// The edge kind, when this reference maps straight onto one.
    pub const fn edge_kind(self) -> Option<EdgeKind> {
        match self {
            ReferenceKind::Edge(k) => Some(k),
            ReferenceKind::FunctionRef => None,
        }
    }
}

impl From<EdgeKind> for ReferenceKind {
    fn from(k: EdgeKind) -> Self {
        ReferenceKind::Edge(k)
    }
}

impl FromStr for ReferenceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown ReferenceKind: {s}"))
    }
}

impl fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ReferenceKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReferenceKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| de::Error::custom(format!("unknown ReferenceKind: {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tables_match_the_typescript_wire_order() {
        // src/types.ts NODE_KINDS / EDGE_KINDS — the order is the kernel's wire contract.
        let nodes: Vec<&str> = NodeKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            nodes,
            [
                "file", "module", "class", "struct", "interface", "trait", "protocol", "function",
                "method", "property", "field", "variable", "constant", "enum", "enum_member",
                "type_alias", "namespace", "parameter", "import", "export", "route", "component",
                "union",
            ]
        );
        let edges: Vec<&str> = EdgeKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            edges,
            [
                "contains", "calls", "imports", "exports", "extends", "implements", "references",
                "type_of", "returns", "instantiates", "overrides", "decorates", "navigates",
            ]
        );
        assert_eq!(Language::ALL.len(), 42);
    }

    #[test]
    fn round_trips_through_strings_and_json() {
        for k in NodeKind::ALL {
            assert_eq!(NodeKind::parse(k.as_str()), Some(*k));
            assert_eq!(k.index() as usize, NodeKind::ALL.iter().position(|x| x == k).unwrap());
        }
        for l in Language::ALL {
            let json = serde_json::to_string(l).unwrap();
            assert_eq!(serde_json::from_str::<Language>(&json).unwrap(), *l);
        }
        assert!(serde_json::from_str::<Language>("\"cobalt\"").is_err());
        assert_eq!(
            serde_json::to_string(&Provenance::TreeSitter).unwrap(),
            "\"tree-sitter\""
        );
    }

    #[test]
    fn reference_kind_is_edge_kind_or_function_ref() {
        assert_eq!(ReferenceKind::parse("calls"), Some(EdgeKind::Calls.into()));
        assert_eq!(ReferenceKind::parse("function_ref"), Some(ReferenceKind::FunctionRef));
        assert_eq!(ReferenceKind::parse("nope"), None);
        assert_eq!(ReferenceKind::FunctionRef.edge_kind(), None);
    }
}
