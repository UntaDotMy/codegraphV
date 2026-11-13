//! C# type arguments and keyword types decide what a name means; extension methods on keyword
//! receivers (upstream 092e6cf8; fixture from csharp-declared-receivers.test.ts).
use codegraph_core::CodeGraph;
use codegraph_types::EdgeKind;
use std::fs;

const FILES: &[(&str, &str)] = &[
    ("src/Pages/List.razor.cs", r#"using System.Collections.Generic;

namespace App.Pages;

public partial class List
{
    private List<Brand> brands = new List<Brand>();
    private List list;
}

public class Brand
{
}
"#),
    ("src/Pages/List.razor", r#"<h1>List</h1>
"#),
    ("src/Pages/Details.razor", r#"@page "/details"
@code {
    private List<Brand> _members = new List<Brand>();
}
"#),
    ("src/Probing/IProbe.cs", r#"namespace App.Probing;

public interface IProbe
{
}

public interface IProbe<T>
{
}

public class Poller
{
    public void Check(IProbe probe) { }
    public T Get<T>(IProbe<T> probe) { return default; }
    public void Pair(IProbe<int, string> probe) { }
}
"#),
    ("src/Entities/BaseItem.cs", r#"namespace App.Entities;

public class BaseItem
{
    public override string ToString() { return ""; }
    public string GetParent() { return ""; }
}

public class Year : BaseItem
{
}
"#),
    ("src/Clients/TmdbClient.cs", r#"using System.Globalization;
using App.Entities;

namespace App.Clients;

public class TmdbClient
{
    public string Search(string name, int year = 0)
    {
        return name + year.ToString(CultureInfo.InvariantCulture);
    }

    public void Loop(int[] years)
    {
        foreach (int year in years)
        {
            year.ToString(CultureInfo.InvariantCulture);
        }
    }

    public string Guess(object item)
    {
        var year = item as Year;
        return year.GetParent();
    }

    public string Named(Year year)
    {
        return year.GetParent();
    }
}
"#),
    ("src/Formatting/Formatter.cs", r#"using System.Globalization;

namespace App.Formatting;

public abstract class PropertyValue
{
    public abstract string ToString(string format);
}

public class Formatter
{
    public void Format(PropertyValue value)
    {
    }

    static string FormatDecimal(decimal value)
    {
        return value.ToString(CultureInfo.InvariantCulture);
    }

    static string Text(string text)
    {
        return text.Shout();
    }
}

public static class StringExtensions
{
    public static string Shout(this string text)
    {
        return text;
    }
}
"#),
    ("src/Support/Extensions.cs", r#"using System.Collections.Generic;
using System.Linq;

namespace App.Support;

public class Value
{
    public List<Value> Elements;
}

public static class Extensions
{
    public static object Literal(this Value value)
    {
        return string.Join(",", value.Elements.Select(e => e.Literal()));
    }

    public static object Wrapped(this Value value, Holder holder)
    {
        return holder.Inner.Wrapped();
    }
}

public class Holder
{
    public object Inner;
}
"#),
    ("src/Support/Strings.cs", r#"using System;
using System.Diagnostics;
using System.Diagnostics.CodeAnalysis;
using System.Linq;
using System.Runtime.CompilerServices;

namespace App.Support;

public static class Strings
{
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static string Inlined(this string text)
    {
        return text;
    }

    [Obsolete("Stray [ bracket")]
    // Kept for the old callers.
    [MethodImpl(MethodImplOptions.AggressiveInlining)] [DebuggerStepThrough]
    public static T Stacked<T>(this T text)
    {
        return text;
    }

    public static string Truncated(
        this string text,
        int length)
    {
        return text;
    }

    public static bool IsMissing([NotNullWhen(false)] this string? text)
    {
        return text == null;
    }

    public static void Bumped(ref this int number)
    {
    }

    public static (string First, string Rest) Cut(this string text)
    {
        return (text, text);
    }

    [Obsolete("Use Plain(this string text)")]
    public static string Plain(string text)
    {
        return text;
    }
}

public class Wrapper
{
    public string Self(string text) => Wrap(this as object);

    private string Wrap(object value) => "";
}

public class Caller
{
    public void Call(string text, int number)
    {
        text.Inlined();
        text.Stacked();
        text.Truncated(3);
        text.IsMissing();
        text.Cut();
        number.Bumped();
        text.Plain();
        text.Self();
    }
}

public static class Trees
{
    [DebuggerStepThrough]
    public static object Flattened(this Value value)
    {
        return value.Elements.Select(e => e.Flattened());
    }

    public static object Unwound(
        this Value value)
    {
        return value.Elements.Select(e => e.Unwound());
    }

    public static object Spread(this Value value)
    {
        return value.Elements.Select(e =>
            e.Spread());
    }
}
"#),
    ("src/Probing/Attributed.cs", r#"using System;
using System.ComponentModel;

namespace App.Probing;

[Serializable]
[Category("one")]
[Category("two")]
[Category("three")]
[Category("four")]
[Category("five")]
[Category("six")]
[Category("seven")]
public class Cache<T>
{
}

[Description("class Store<T>")]
public class Store
{
    // Not a class Store<T> of any kind.
}

public class Pool
{
    // See class Pool<T>.
}

public class Warehouse
{
    private Cache<int> cache = new Cache<int>();
    private Store<int> store = new Store<int>();
    private Pool<int> pool = new Pool<int>();
}
"#),
];

/// (kind, source qualified name, target qualified name, line, refName metadata)
type Row = (EdgeKind, String, String, Option<u32>, Option<String>);

fn edges(cg: &CodeGraph) -> Vec<Row> {
    let nodes = cg.get_all_nodes().unwrap();
    let by_id: std::collections::HashMap<&str, &str> = nodes.iter().map(|n| (n.id.as_str(), n.qualified_name.as_str())).collect();
    let mut out = Vec::new();
    for n in &nodes {
        for e in cg.get_outgoing_edges(&n.id).unwrap() {
            let Some(target) = by_id.get(e.target.as_str()) else { continue };
            let ref_name = e.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str()).map(String::from);
            out.push((e.kind, n.qualified_name.clone(), target.to_string(), e.line, ref_name));
        }
    }
    out
}

fn callees(rows: &[Row], caller: &str, reference: &str) -> Vec<String> {
    rows.iter().filter(|r| r.0 == EdgeKind::Calls && r.1 == caller && r.4.as_deref() == Some(reference)).map(|r| r.2.clone()).collect()
}

#[test]
fn csharp_type_arguments_and_keyword_types_decide_what_a_name_means() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, text) in FILES {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    let rows = edges(&cg);
    let mut failed: Vec<String> = Vec::new();
    let mut eq = |label: &str, got: Vec<String>, want: &[&str]| {
        if got != want.iter().map(|s| s.to_string()).collect::<Vec<_>>() {
            failed.push(format!("{label}: got {got:?}, want {want:?}"));
        }
    };
    let typed = |k: &EdgeKind| *k == EdgeKind::References || *k == EdgeKind::Instantiates;

    // a name written with type arguments is no type that declares none
    let to_list: Vec<String> = rows
        .iter()
        .filter(|r| typed(&r.0) && (r.2 == "List" || r.2.ends_with("::List")))
        .map(|r| format!("{} {} {}", r.0.as_str(), r.1, r.3.unwrap_or(0)))
        .collect();
    eq("List", to_list, &["references App.Pages::List::list 8"]);
    let mut probe: Vec<String> = rows.iter().filter(|r| r.0 == EdgeKind::References && r.2 == "App.Probing::IProbe").map(|r| r.1.clone()).collect();
    probe.sort();
    eq("IProbe sources", probe, &["App.Probing::Poller::Check", "App.Probing::Poller::Get"]);
    let mut probe_lines: Vec<String> = rows.iter().filter(|r| r.0 == EdgeKind::References && r.2 == "App.Probing::IProbe").map(|r| r.3.unwrap_or(0).to_string()).collect();
    probe_lines.sort();
    eq("IProbe lines", probe_lines, &["13", "14"]);
    eq("Pair", rows.iter().filter(|r| r.1 == "App.Probing::Poller::Pair" && r.0 == EdgeKind::References).map(|r| r.2.clone()).collect(), &[]);

    // a type with attributes above it
    let links = |ty: &str| -> Vec<String> {
        let mut k: Vec<String> = rows.iter().filter(|r| typed(&r.0) && r.2 == ty).map(|r| r.0.as_str().to_string()).collect();
        k.sort();
        k
    };
    eq("Cache", links("App.Probing::Cache"), &["instantiates", "references"]);
    eq("Store", links("App.Probing::Store"), &[]);
    eq("Pool", links("App.Probing::Pool"), &[]);

    // a receiver declared with a keyword type
    eq("Search year", callees(&rows, "App.Clients::TmdbClient::Search", "year.ToString"), &[]);
    eq("Loop year", callees(&rows, "App.Clients::TmdbClient::Loop", "year.ToString"), &[]);
    eq("FormatDecimal", callees(&rows, "App.Formatting::Formatter::FormatDecimal", "value.ToString"), &[]);
    eq("Shout", callees(&rows, "App.Formatting::Formatter::Text", "text.Shout"), &["App.Formatting::StringExtensions::Shout"]);

    // an extension method on a keyword receiver
    let called = |r: &str| callees(&rows, "App.Support::Caller::Call", r);
    eq("Inlined", called("text.Inlined"), &["App.Support::Strings::Inlined"]);
    eq("Stacked", called("text.Stacked"), &["App.Support::Strings::Stacked"]);
    eq("Truncated", called("text.Truncated"), &["App.Support::Strings::Truncated"]);
    eq("IsMissing", called("text.IsMissing"), &["App.Support::Strings::IsMissing"]);
    eq("Bumped", called("number.Bumped"), &["App.Support::Strings::Bumped"]);
    eq("Cut", called("text.Cut"), &["App.Support::Strings::Cut"]);
    eq("Plain", called("text.Plain"), &[]);
    eq("Self", called("text.Self"), &[]);

    // a receiver no keyword declares stays named after its class
    eq("Guess", callees(&rows, "App.Clients::TmdbClient::Guess", "year.GetParent"), &["App.Entities::BaseItem::GetParent"]);
    eq("Named", callees(&rows, "App.Clients::TmdbClient::Named", "year.GetParent"), &["App.Entities::BaseItem::GetParent"]);

    // an extension method calling its own name
    eq("Literal", callees(&rows, "App.Support::Extensions::Literal", "e.Literal"), &["App.Support::Extensions::Literal"]);
    eq("Flattened", callees(&rows, "App.Support::Trees::Flattened", "e.Flattened"), &["App.Support::Trees::Flattened"]);
    eq("Unwound", callees(&rows, "App.Support::Trees::Unwound", "e.Unwound"), &["App.Support::Trees::Unwound"]);
    eq("Spread", callees(&rows, "App.Support::Trees::Spread", "e.Spread"), &["App.Support::Trees::Spread"]);
    eq("Wrapped", callees(&rows, "App.Support::Extensions::Wrapped", "holder.Inner.Wrapped"), &[]);
    assert!(failed.is_empty(), "{} case(s) failed:\n{}", failed.len(), failed.join("\n"));
    cg.close();
}
