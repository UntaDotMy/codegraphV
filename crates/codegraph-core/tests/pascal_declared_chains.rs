//! Delphi chains, method pointers, nested scopes and forward routines
//! (upstream 063708ba, #3065; the cases reimplement part of PR #2550 by
//! @OleksiyPenkov).
//!
//! A chain such as `TLogger2.Create.Write('y')` links the method of the type
//! each step is declared to give. A chain through a type the project does not
//! declare links nothing. A method given to a routine-typed target is a
//! reference. A nested routine keeps its locals. A name list types every name
//! in it. A pointer reaches the type it names. A nested routine declared
//! `forward` is in scope from that declaration.
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;

struct Fixture {
    cg: CodeGraph,
    _root: tempfile::TempDir,
}

fn index(files: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for (relative, content) in files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    Fixture { cg, _root: root }
}

fn links_of(cg: &CodeGraph, file: &str, qualified: &str) -> Vec<String> {
    let nodes = cg.get_nodes_by_file(file).unwrap();
    let mut out = Vec::new();
    let mut found = false;
    for caller in nodes.iter().filter(|n| {
        n.qualified_name == qualified && matches!(n.kind, NodeKind::Function | NodeKind::Method)
    }) {
        found = true;
        for edge in cg.get_outgoing_edges(&caller.id).unwrap() {
            if edge.kind != EdgeKind::Calls && edge.kind != EdgeKind::References {
                continue;
            }
            let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
            let kind = if edge.kind == EdgeKind::Calls { "calls" } else { "references" };
            let line = edge.line.unwrap_or(0);
            out.push(format!("{line} {kind} {}", target.qualified_name));
        }
    }
    if !found {
        let have: Vec<&str> = nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
            .map(|n| n.qualified_name.as_str())
            .collect();
        return vec![format!("<no {qualified} in {file}; have {}>", have.join(", "))];
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn constructor_chain_and_outside_type() {
    let wire = index(&[(
        "Wire.pas",
        "unit Wire;\ninterface\ntype\n TLogger2 = class procedure Emit(const S: string); procedure Write(const S: string); end;\n TBuilder = class constructor Make(N: Integer); procedure Build; end;\n TDecoy = class procedure Emit(const S: string); procedure Write(const S: string); procedure Build; end;\n TWirer = class procedure CallEmit; end;\nimplementation\nprocedure TLogger2.Emit(const S: string); begin end;\nprocedure TLogger2.Write(const S: string); begin end;\nconstructor TBuilder.Make(N: Integer); begin end;\nprocedure TBuilder.Build; begin end;\nprocedure TDecoy.Emit(const S: string); begin end;\nprocedure TDecoy.Write(const S: string); begin end;\nprocedure TDecoy.Build; begin end;\nprocedure TWirer.CallEmit;\nbegin\n  TLogger2.Create.Emit('x');\n  TLogger2.Create.Write('y');\n  TBuilder.Make(1).Build;\nend;\nend.\n",
    )]);
    assert_eq!(
        links_of(&wire.cg, "Wire.pas", "TWirer::CallEmit"),
        vec![
            "18 calls TLogger2::Emit".to_string(),
            "19 calls TLogger2::Write".to_string(),
            "20 calls TBuilder::Build".to_string(),
            "20 calls TBuilder::Make".to_string(),
        ]
    );

    let outside = index(&[(
        "Main.pas",
        "unit Main;\ninterface\nuses Vcl.ExtCtrls;\ntype\n TDecoy = class procedure Clear; procedure Refresh(N: Integer); end;\n TOwner = class\n  FPanel: TPanel;\n  procedure Run;\n end;\nimplementation\nprocedure TDecoy.Clear; begin end;\nprocedure TDecoy.Refresh(N: Integer); begin end;\nprocedure TOwner.Run;\nbegin\n  FPanel.Controls.Clear;\n  FPanel.Parent.Refresh(1);\nend;\nend.\n",
    )]);
    assert_eq!(links_of(&outside.cg, "Main.pas", "TOwner::Run"), Vec::<String>::new());
}

#[test]
fn routine_typed_assignment_is_a_reference() {
    let locals = index(&[(
        "Wire.pas",
        "unit Wire;\ninterface\ntype\n TGet = function: Integer of object;\n TGetAlias = TGet;\n TProvider = class function Value: Integer; end;\n TWirer = class\n  FProvider: TProvider;\n  procedure Locals;\n end;\nimplementation\nfunction TProvider.Value: Integer; begin Result := 1; end;\nprocedure TWirer.Locals;\nvar\n  Named: TGet;\n  Aliased: TGetAlias;\n  Written: function: Integer of object;\n  Closure: reference to function: Integer;\n  Number: Integer;\nbegin\n  Named := FProvider.Value;\n  Aliased := FProvider.Value;\n  Written := FProvider.Value;\n  Closure := FProvider.Value;\n  Number := FProvider.Value;\nend;\nend.\n",
    )]);
    assert_eq!(
        links_of(&locals.cg, "Wire.pas", "TWirer::Locals"),
        vec![
            "21 references TProvider::Value".to_string(),
            "22 references TProvider::Value".to_string(),
            "23 references TProvider::Value".to_string(),
            "24 references TProvider::Value".to_string(),
            "25 calls TProvider::Value".to_string(),
        ]
    );

    let at = index(&[(
        "At.pas",
        "unit At;\ninterface\ntype\n TGet = function: Integer of object;\n TProvider = class function Value: Integer; end;\n TWirer = class\n  FProvider: TProvider;\n  Callback: TGet;\n  FOnGet: TGet;\n  function Own: Integer;\n  procedure AtOnNamed;\n  procedure AtCallback;\n  procedure AtBareOwn;\n end;\nimplementation\nfunction TProvider.Value: Integer; begin Result := 1; end;\nfunction TWirer.Own: Integer; begin Result := 2; end;\nprocedure TWirer.AtOnNamed; begin FOnGet := @FProvider.Value; end;\nprocedure TWirer.AtCallback; begin Callback := @FProvider.Value; end;\nprocedure TWirer.AtBareOwn; begin Callback := @Own; end;\nend.\n",
    )]);
    assert_eq!(links_of(&at.cg, "At.pas", "TWirer::AtOnNamed"), vec!["18 references TProvider::Value".to_string()]);
    assert_eq!(links_of(&at.cg, "At.pas", "TWirer::AtCallback"), vec!["19 references TProvider::Value".to_string()]);
    assert_eq!(links_of(&at.cg, "At.pas", "TWirer::AtBareOwn"), vec!["20 references TWirer::Own".to_string()]);

    let data = index(&[(
        "Data.pas",
        "unit Data;\ninterface\ntype\n TProvider = class function Value: Integer; function IsTop: Boolean; end;\n TWirer = class\n  FProvider: TProvider;\n  FOnCount: Integer;\n  FOnTop: Boolean;\n  procedure Wire;\n end;\nimplementation\nfunction TProvider.Value: Integer; begin Result := 1; end;\nfunction TProvider.IsTop: Boolean; begin Result := True; end;\nprocedure TWirer.Wire;\nbegin\n  FOnCount := FProvider.Value;\n  FOnTop := FProvider.IsTop;\nend;\nend.\n",
    )]);
    assert_eq!(
        links_of(&data.cg, "Data.pas", "TWirer::Wire"),
        vec!["16 calls TProvider::Value".to_string(), "17 calls TProvider::IsTop".to_string()]
    );
}

#[test]
fn nested_locals_name_lists_and_pointers() {
    let nested = index(&[(
        "Scope.pas",
        "unit Scope;\ninterface\ntype\n TField = class procedure Run; end;\n TLocal = class procedure Run; end;\n TScoped = class Receiver: TField; procedure UseLocalNested; end;\nimplementation\nprocedure TField.Run; begin end;\nprocedure TLocal.Run; begin end;\nprocedure TScoped.UseLocalNested;\n procedure Inner;\n var Receiver: TLocal;\n begin Receiver.Run; end;\nbegin Receiver.Run; end;\nend.\n",
    )]);
    assert_eq!(
        links_of(&nested.cg, "Scope.pas", "TScoped::UseLocalNested::Inner"),
        vec!["13 calls TLocal::Run".to_string()]
    );
    assert_eq!(
        links_of(&nested.cg, "Scope.pas", "TScoped::UseLocalNested"),
        vec!["14 calls TField::Run".to_string()]
    );

    let listed = index(&[(
        "Docs.pas",
        "unit Docs;\ninterface\ntype\n TDocData = class procedure Clear; end;\n TOther = class procedure Clear; end;\n TItem = class procedure Clear; end;\n TTests = class procedure Run; end;\nimplementation\nprocedure TDocData.Clear; begin end;\nprocedure TOther.Clear; begin end;\nprocedure TItem.Clear; begin end;\nprocedure TTests.Run;\n  procedure Check(var Item: TOther);\n  begin\n    Item.Clear;\n  end;\nvar\n  Item, Spare: TDocData;\nbegin\n  Item.Clear;\n  Spare.Clear;\nend;\nend.\n",
    )]);
    assert_eq!(links_of(&listed.cg, "Docs.pas", "TTests::Run::Check"), vec!["15 calls TOther::Clear".to_string()]);
    assert_eq!(
        links_of(&listed.cg, "Docs.pas", "TTests::Run"),
        vec!["20 calls TDocData::Clear".to_string(), "21 calls TDocData::Clear".to_string()]
    );

    let pointer = index(&[(
        "Tree.pas",
        "unit Tree;\ninterface\ntype\n  PNode = ^TNode;\n  TNode = record\n    LastChild: PNode;\n    procedure SetNextSibling(Value: PNode);\n  end;\n  TDecoy = class\n    procedure SetNextSibling(Value: Pointer);\n  end;\nprocedure Attach(Node, Child: PNode);\nimplementation\nprocedure TNode.SetNextSibling(Value: PNode); begin end;\nprocedure TDecoy.SetNextSibling(Value: Pointer); begin end;\nprocedure Attach(Node, Child: PNode);\nbegin\n  Node.LastChild.SetNextSibling(Child);\nend;\nend.\n",
    )]);
    assert_eq!(links_of(&pointer.cg, "Tree.pas", "Attach"), vec!["18 calls TNode::SetNextSibling".to_string()]);
}

#[test]
fn forward_nested_routine_is_in_scope() {
    let fx = index(&[(
        "Scope.pas",
        "unit Scope;\ninterface\ntype\n TScoped = class procedure Fire; end;\nimplementation\nprocedure TScoped.Fire;\n  procedure Late; forward;\n  function Later(N: Integer;\n    M: Integer): Integer; forward;\n  procedure Early;\n  begin\n    Late;\n    Later(1, 2);\n  end;\n  procedure Late; begin end;\n  function Later(N: Integer; M: Integer): Integer; begin Result := N + M; end;\nbegin\n  Early;\nend;\nend.\n",
    )]);
    assert_eq!(
        links_of(&fx.cg, "Scope.pas", "TScoped::Fire::Early"),
        vec!["12 calls TScoped::Fire::Late".to_string(), "13 calls TScoped::Fire::Later".to_string()]
    );
    assert_eq!(links_of(&fx.cg, "Scope.pas", "TScoped::Fire"), vec!["18 calls TScoped::Fire::Early".to_string()]);
}
