//! Drupal's dependency injection, plugin definitions and OOP hooks, indexed (upstream a2870563;
//! port of the indexed part of drupal-services.test.ts — the extract-level cases live in
//! codegraph-resolve's `frameworks::drupal::services_tests`).
use codegraph_core::{CodeGraph, IndexOptions};
use codegraph_types::{EdgeKind, NodeKind, SearchOptions};
use std::fs;
use std::path::Path;

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn node_id(cg: &CodeGraph, name: &str, kind: NodeKind) -> String {
    cg.get_nodes_by_name(name).unwrap().into_iter().find(|n| n.kind == kind).unwrap_or_else(|| panic!("no {kind:?} {name}")).id
}

fn outgoing(cg: &CodeGraph, name: &str, kind: NodeKind) -> Vec<String> {
    let mut out: Vec<String> = cg
        .get_outgoing_edges(&node_id(cg, name, kind))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EdgeKind::References)
        .map(|e| cg.get_node(&e.target).unwrap().unwrap().name)
        .collect();
    out.sort();
    out
}

fn module(dir: &Path) {
    write(dir, "composer.json", r#"{"name":"drupal/shop","type":"drupal-module"}"#);
    write(dir, "shop.info.yml", "name: Shop\ntype: module\n");
    write(dir, "shop.services.yml", r"services:
  shop.cart:
    class: Drupal\shop\Cart
    arguments: ['@shop.prices', '@database']
  shop.prices:
    class: Drupal\shop\Prices
  shop.subscriber:
    class: Drupal\shop\EventSubscriber\CartSubscriber
    tags:
      - { name: event_subscriber }
  Drupal\shop\Autowired: ~
  shop.elsewhere:
    class: Drupal\elsewhere\Prices
");
    write(dir, "src/Cart.php", "<?php\nnamespace Drupal\\shop;\nclass Cart extends CartBase {\n  public function total() { return 1; }\n}\n");
    write(dir, "src/CartBase.php", "<?php\nnamespace Drupal\\shop;\nclass CartBase {\n  public function clear() {}\n}\n");
    write(dir, "src/Prices.php", "<?php\nnamespace Drupal\\shop;\nclass Prices {\n  public function total() { return 2; }\n}\n");
    write(dir, "src/Autowired.php", "<?php\nnamespace Drupal\\shop;\nclass Autowired {}\n");
    // A class of the same name in another namespace: the id's namespace decides.
    write(dir, "src/Other/Prices.php", "<?php\nnamespace Drupal\\shop\\Other;\nclass Prices {}\n");
    write(dir, "src/EventSubscriber/CartSubscriber.php", "<?php\nnamespace Drupal\\shop\\EventSubscriber;\nclass CartSubscriber {}\n");
    write(dir, "src/Checkout.php", r"<?php
namespace Drupal\shop;
class Checkout {
  public function run($container) {
    \Drupal::service('shop.cart')->total();
    \Drupal::service('shop.cart')->clear();
    $container->get('shop.prices')->total();
    \Drupal::service('no.such.service')->total();
    \Drupal::service('shop.cart')->missing();
  }
}
");
    write(dir, "src/Plugin/Block/CartBlock.php", r"<?php
namespace Drupal\shop\Plugin\Block;
#[Block(id: 'shop_cart', admin_label: new TranslatableMarkup('Cart'))]
class CartBlock extends BlockBase {
  public function build() { return []; }
}
");
    write(dir, "src/Plugin/Block/OldBlock.php", r#"<?php
namespace Drupal\shop\Plugin\Block;
/**
 * @Block(id = "shop_old", admin_label = @Translation("Old"))
 */
class OldBlock extends BlockBase {}
"#);
    write(dir, "src/Hook/ShopHooks.php", r"<?php
namespace Drupal\shop\Hook;
class ShopHooks {
  /**
   * Implements hook_shop_ready().
   */
  #[Hook('shop_ready')]
  public function ready(): void {}

  #[Hook('form_alter')]
  public function formAlter(): void {}
}
");
    // A plugin whose id is a class the project uses, and a service whose id is a class.
    write(dir, "src/Plugin/Validation/Constraint/CurrencyConstraint.php", "<?php\nnamespace Drupal\\shop\\Plugin\\Validation\\Constraint;\n#[Constraint(id: 'Currency')]\nclass CurrencyConstraint {}\n");
    write(dir, "src/Rates.php", r"<?php
namespace Drupal\shop;
use Drupal\shop\Autowired;
use Vendor\Money\Currency;
class Rates {
  public function make() {
    return new Currency();
  }
}
");
    write(dir, "shop.api.php", "<?php\nfunction hook_shop_ready() {}\n");
    write(dir, "shop.module", "<?php\nfunction shop_form_alter() {}\n");
}

#[test]
fn a_drupal_module_indexed() {
    let dir = tempfile::tempdir().unwrap();
    module(dir.path());
    let cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();

    // A service links to its class, by namespace, and to the services it is given.
    assert_eq!(outgoing(&cg, "service:shop.cart", NodeKind::Component), ["Cart", "service:shop.prices"]);
    let prices = node_id(&cg, "service:shop.prices", NodeKind::Component);
    let target = cg.get_outgoing_edges(&prices).unwrap().into_iter().find(|e| e.kind == EdgeKind::References).map(|e| cg.get_node(&e.target).unwrap().unwrap()).unwrap();
    assert_eq!(target.file_path, "src/Prices.php");
    // Nothing for a service whose class isn't indexed, whatever shares its short name.
    assert!(outgoing(&cg, "service:shop.elsewhere", NodeKind::Component).is_empty());
    assert_eq!(outgoing(&cg, "service:Drupal\\shop\\Autowired", NodeKind::Component), ["Autowired"]);
    assert_eq!(outgoing(&cg, "service:shop.subscriber", NodeKind::Component), ["CartSubscriber"]);

    // A call through the container reaches the method on the service's class.
    let mut run: Vec<String> = cg.get_callees(&node_id(&cg, "run", NodeKind::Method), 1).unwrap().into_iter().map(|c| c.node.qualified_name).collect();
    run.sort();
    assert_eq!(run, ["Drupal\\shop::Cart::total", "Drupal\\shop::CartBase::clear", "Drupal\\shop::Prices::total"]);

    // A service or plugin id doesn't answer for the class of that name.
    let make = node_id(&cg, "make", NodeKind::Method);
    assert!(cg.get_outgoing_edges(&make).unwrap().iter().all(|e| e.kind != EdgeKind::Instantiates));
    let imported: Vec<_> = cg
        .get_nodes_by_file("src/Rates.php")
        .unwrap()
        .into_iter()
        .flat_map(|n| cg.get_outgoing_edges(&n.id).unwrap())
        .filter(|e| e.kind == EdgeKind::Imports)
        .map(|e| cg.get_node(&e.target).unwrap().unwrap())
        .collect();
    assert!(imported.iter().all(|n| n.kind != NodeKind::Component), "{imported:?}");
    assert!(imported.iter().any(|n| n.name == "Autowired"), "{imported:?}");

    // Search finds a service and a plugin by the id written in the project.
    let found = |q: &str| cg.search_nodes(q, &SearchOptions::default()).unwrap().into_iter().map(|r| r.node.name).collect::<Vec<_>>();
    assert!(found("shop.prices").contains(&"service:shop.prices".to_string()), "{:?}", found("shop.prices"));
    assert!(found("shop_cart").contains(&"Block:shop_cart".to_string()), "{:?}", found("shop_cart"));

    // A plugin links to its class, from an attribute and from an annotation.
    assert_eq!(outgoing(&cg, "Block:shop_cart", NodeKind::Component), ["CartBlock"]);
    assert_eq!(outgoing(&cg, "Block:shop_old", NodeKind::Component), ["OldBlock"]);

    // A #[Hook] method links to the hook when the index defines it, and to no other module's function.
    assert_eq!(outgoing(&cg, "ready", NodeKind::Method), ["hook_shop_ready"]);
    assert!(outgoing(&cg, "formAlter", NodeKind::Method).is_empty());
    cg.close();
}

#[test]
fn a_sync_links_a_service_when_its_class_arrives() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "composer.json", r#"{"name":"drupal/shop","type":"drupal-module"}"#);
    write(dir.path(), "shop.services.yml", "services:\n  shop.cart:\n    class: Drupal\\shop\\Cart\n");
    let mut cg = CodeGraph::init_and_index(dir.path().to_str().unwrap(), None).unwrap();
    assert!(outgoing(&cg, "service:shop.cart", NodeKind::Component).is_empty());
    write(dir.path(), "src/Cart.php", "<?php\nnamespace Drupal\\shop;\nclass Cart {}\n");
    cg.sync(&IndexOptions::default(), None).unwrap();
    assert_eq!(outgoing(&cg, "service:shop.cart", NodeKind::Component), ["Cart"]);
    cg.close();
}
