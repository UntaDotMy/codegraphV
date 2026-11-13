//! A PHP receiver reaches the class the file can name, through its traits and
//! then the parents that file imports (upstream 14c25e37, #3168).
use codegraph_core::CodeGraph;
use codegraph_types::{EdgeKind, NodeKind};
use std::fs;
use std::path::Path;

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "views/HandlerBase.php",
            "<?php\nnamespace Drupal\\views\\Plugin\\views;\n\nabstract class HandlerBase\n{\n    public function access($account) { return true; }\n    public function getEntityType() { return ''; }\n}\n",
        ),
        (
            "views/AreaPluginBase.php",
            "<?php\nnamespace Drupal\\views\\Plugin\\views\\area;\n\nuse Drupal\\views\\Plugin\\views\\HandlerBase;\n\nabstract class AreaPluginBase extends HandlerBase\n{\n}\n",
        ),
        (
            "views/TokenizeAreaPluginBase.php",
            "<?php\nnamespace Drupal\\views\\Plugin\\views\\area;\n\nabstract class TokenizeAreaPluginBase extends AreaPluginBase\n{\n}\n",
        ),
        (
            "views/Entity.php",
            "<?php\nnamespace Drupal\\views\\Plugin\\views\\area;\n\nclass Entity extends TokenizeAreaPluginBase\n{\n}\n",
        ),
        (
            "entity/AccessibleInterface.php",
            "<?php\nnamespace Drupal\\Core\\Access;\n\ninterface AccessibleInterface\n{\n    public function access($operation, $account = NULL);\n}\n",
        ),
        (
            "entity/EntityInterface.php",
            "<?php\nnamespace Drupal\\Core\\Entity;\n\nuse Drupal\\Core\\Access\\AccessibleInterface;\n\ninterface EntityInterface extends AccessibleInterface\n{\n    public function getEntityType();\n}\n",
        ),
        (
            "entity/ContentEntityBase.php",
            "<?php\nnamespace Drupal\\Core\\Entity;\n\nabstract class ContentEntityBase implements EntityInterface\n{\n    public function access($operation, $account = NULL) { return true; }\n    public function getEntityType() { return ''; }\n}\n",
        ),
        (
            "entity/Controller.php",
            "<?php\nnamespace Drupal\\comment\\Controller;\n\nuse Drupal\\Core\\Entity\\EntityInterface;\n\nclass CommentController\n{\n    public function permalink($comment)\n    {\n        $entity = $comment->getCommentedEntity();\n        return $entity->access('view');\n    }\n\n    public function replyAccess(EntityInterface $entity)\n    {\n        return $entity->access('view');\n    }\n\n    public function typeOf(EntityInterface $entity)\n    {\n        return $entity->getEntityType();\n    }\n}\n",
        ),
        (
            "group/Page.php",
            "<?php\nnamespace App\\Models;\n\nuse App\\Concerns\\Saves;\n\nclass Page\n{\n    use Saves;\n}\n",
        ),
        (
            "group/OtherPage.php",
            "<?php\nnamespace App\\Legacy;\n\nclass Page\n{\n}\n",
        ),
        (
            "group/Saves.php",
            "<?php\nnamespace App\\Concerns;\n\ntrait Saves\n{\n    public function persist() { return true; }\n}\n",
        ),
        (
            "group/Controller.php",
            "<?php\nnamespace App\\Http;\n\nuse App\\Models\\{Book, Page};\n\nclass PageController\n{\n    public function store($page)\n    {\n        return $page->persist();\n    }\n}\n",
        ),
        (
            "view/Renderable.php",
            "<?php\nnamespace Illuminate\\Contracts\\Support;\n\ninterface Renderable\n{\n    public function render();\n}\n",
        ),
        (
            "view/ContractView.php",
            "<?php\nnamespace Illuminate\\Contracts\\View;\n\nuse Illuminate\\Contracts\\Support\\Renderable;\n\ninterface View extends Renderable\n{\n}\n",
        ),
        (
            "view/View.php",
            "<?php\nnamespace Illuminate\\View;\n\nclass View\n{\n    public function render() { return ''; }\n}\n",
        ),
        (
            "view/Compiler.php",
            "<?php\nnamespace Illuminate\\View\\Compilers;\n\nuse Illuminate\\View\\View;\n\nclass BladeCompiler\n{\n    public function compile($view)\n    {\n        return $view->render();\n    }\n}\n",
        ),
        (
            "pivot/Model.php",
            "<?php\nnamespace Illuminate\\Database\\Eloquent;\n\nabstract class Model\n{\n    public function getForeignKey() { return ''; }\n    public function getKey() { return 1; }\n}\n",
        ),
        (
            "pivot/AsPivot.php",
            "<?php\nnamespace Illuminate\\Database\\Eloquent\\Relations\\Concerns;\n\ntrait AsPivot\n{\n    public function getForeignKey() { return ''; }\n}\n",
        ),
        (
            "pivot/Pivot.php",
            "<?php\nnamespace Illuminate\\Database\\Eloquent\\Relations;\n\nuse Illuminate\\Database\\Eloquent\\Model;\nuse Illuminate\\Database\\Eloquent\\Relations\\Concerns\\AsPivot;\n\nclass Pivot extends Model\n{\n    use AsPivot;\n}\n",
        ),
        (
            "pivot/PivotTest.php",
            "<?php\nnamespace Illuminate\\Tests;\n\nuse Illuminate\\Database\\Eloquent\\Relations\\Pivot;\n\nclass PivotUser\n{\n    public function foreignKey($pivot)\n    {\n        return $pivot->getForeignKey();\n    }\n\n    public function key($pivot)\n    {\n        return $pivot->getKey();\n    }\n}\n",
        ),
        (
            "collection/ASupportCollection.php",
            "<?php\nnamespace Illuminate\\Support;\n\nclass Collection\n{\n    public function pluck($value) { return $this; }\n}\n",
        ),
        (
            "collection/EloquentCollection.php",
            "<?php\nnamespace Illuminate\\Database\\Eloquent;\n\nuse Illuminate\\Support\\Collection as BaseCollection;\n\nclass Collection extends BaseCollection\n{\n    public function pluck($value) { return parent::pluck($value); }\n}\n",
        ),
        (
            "collection/Comments.php",
            "<?php\nnamespace Illuminate\\Types\\Model;\n\nuse Illuminate\\Database\\Eloquent\\Collection;\n\nfinal class Comments extends Collection\n{\n    public function firstId($comments)\n    {\n        return $comments->pluck('id');\n    }\n}\n",
        ),
        (
            "facade/Facade.php",
            "<?php\nnamespace Illuminate\\Support\\Facades;\n\nabstract class Facade\n{\n    public static function __callStatic($method, $args) { return null; }\n}\n",
        ),
        (
            "facade/ParallelTestingFacade.php",
            "<?php\nnamespace Illuminate\\Support\\Facades;\n\nclass ParallelTesting extends Facade\n{\n    protected static function getFacadeAccessor()\n    {\n        return \\Illuminate\\Testing\\ParallelTesting::class;\n    }\n}\n",
        ),
        (
            "facade/CacheFacade.php",
            "<?php\nnamespace Illuminate\\Support\\Facades;\n\nclass Cache extends Facade\n{\n    protected static function getFacadeAccessor()\n    {\n        return 'cache';\n    }\n}\n",
        ),
        (
            "facade/Testing/ParallelTesting.php",
            "<?php\nnamespace Illuminate\\Testing;\n\nclass ParallelTesting\n{\n    public function token() { return 1; }\n}\n",
        ),
        (
            "facade/Storage.php",
            "<?php\nnamespace Illuminate\\Support\\Facades;\n\nclass Storage extends Facade\n{\n    public static function fake()\n    {\n        Cache::flushEverything();\n        return ParallelTesting::token();\n    }\n}\n",
        ),
        (
            "facade/Repository.php",
            "<?php\nnamespace App\\Store;\n\nclass Repository\n{\n    public function flushEverything() {}\n}\n",
        ),
        (
            "models/Models.php",
            "<?php\nnamespace App\\Models;\n\nuse Illuminate\\Database\\Eloquent\\Model;\n\nclass Post extends Model\n{\n    public function tags() { return []; }\n}\n\nclass PostStringPrimaryKey extends Model\n{\n    public function tags() { return []; }\n}\n\nclass Str\n{\n    public static function find($x) { return ''; }\n}\n",
        ),
        (
            "models/PostTest.php",
            "<?php\nnamespace App\\Models;\n\nclass PostTest\n{\n    public function testStringKey()\n    {\n        $post = PostStringPrimaryKey::query()->create(['id' => 'a']);\n        return $post->tags();\n    }\n\n    public function testNumericKey()\n    {\n        $post = Post::create([]);\n        return $post->tags();\n    }\n\n    public function testOtherClass()\n    {\n        $post = Str::find('x');\n        return $post->tags();\n    }\n}\n",
        ),
        (
            "theme/Extension.php",
            "<?php\nnamespace Drupal\\Core\\Extension;\n\nclass Extension\n{\n    public function getName() { return ''; }\n}\n\nclass Theme extends Extension\n{\n}\n",
        ),
        (
            "theme/Controller.php",
            "<?php\nnamespace Drupal\\system\\Controller;\n\nclass SystemController\n{\n    public function page($theme)\n    {\n        return $theme->getName();\n    }\n}\n",
        ),
        (
            "chapter/Models.php",
            "<?php\nnamespace BookStack\\Entities\\Models;\n\nabstract class Entity\n{\n    public function getShortName() { return ''; }\n}\n\nabstract class BookChild extends Entity\n{\n}\n\nclass Chapter extends BookChild\n{\n}\n",
        ),
        (
            "chapter/Controller.php",
            "<?php\nnamespace BookStack\\Entities\\Controllers;\n\nclass ChapterController\n{\n    public function show($chapter)\n    {\n        return $chapter->getShortName();\n    }\n}\n",
        ),
        (
            "attributes/EntityBase.php",
            "<?php\nnamespace Drupal\\Core\\Entity;\n\nabstract class EntityBase\n{\n    public function id() { return 1; }\n}\n",
        ),
        (
            "attributes/Term.php",
            "<?php\nnamespace Drupal\\taxonomy\\Entity;\n\nuse Drupal\\Core\\Entity\\EntityBase;\n\n#[ContentEntityType(\n  id: 'taxonomy_term',\n  label: 'Taxonomy term',\n  handlers: [\n    'storage' => 'TermStorage',\n    'view_builder' => 'TermViewBuilder',\n  ],\n  entity_keys: [\n    'id' => 'tid',\n  ],\n)]\nclass Term extends EntityBase\n{\n    public function ownId()\n    {\n        return $this->id();\n    }\n}\n",
        ),
        (
            "attributes/Hooks.php",
            "<?php\nnamespace Drupal\\taxonomy\\Hook;\n\nuse Drupal\\taxonomy\\Entity\\Term;\n\nclass TaxonomyHooks\n{\n    public function deleted(Term $term)\n    {\n        return $term->id();\n    }\n}\n",
        ),
        (
            "outside/BootsTrait.php",
            "<?php\nnamespace App\\Concerns;\n\ntrait BootsTrait\n{\n    public function boot() {}\n}\n",
        ),
        (
            "outside/Provider.php",
            "<?php\nnamespace App\\Providers;\n\nuse Vendor\\Foundation\\ServiceProvider;\n\nclass AppProvider extends ServiceProvider\n{\n    public function register()\n    {\n        parent::boot();\n    }\n\n    public function start()\n    {\n        $this->boot();\n    }\n}\n",
        ),
        (
            "user/User.php",
            "<?php\nnamespace Illuminate\\Foundation\\Auth;\n\nuse Illuminate\\Database\\Eloquent\\Model;\nuse Illuminate\\Auth\\Authenticatable;\n\nclass User extends Model\n{\n    use Authenticatable;\n}\n",
        ),
        (
            "user/Authenticatable.php",
            "<?php\nnamespace Illuminate\\Auth;\n\ntrait Authenticatable\n{\n    public function getAuthIdentifier() { return 1; }\n}\n",
        ),
        (
            "user/Contract.php",
            "<?php\nnamespace Illuminate\\Contracts\\Auth;\n\ninterface Authenticatable\n{\n    public function getAuthIdentifier();\n}\n",
        ),
        (
            "user/OtherUser.php",
            "<?php\nnamespace Illuminate\\Foundation\\Other;\n\nuse Illuminate\\Foundation\\Auth\\User as Authenticatable;\n\nclass User extends Authenticatable\n{\n}\n",
        ),
        (
            "user/BroadcastTest.php",
            "<?php\nnamespace Illuminate\\Tests\\Broadcasting;\n\nclass BroadcasterTest\n{\n    public function testChannel($broadcaster)\n    {\n        return $broadcaster->channel('room', fn ($user) => $user->getAuthIdentifier());\n    }\n}\n\nclass GenericBroadcastingTestUser\n{\n    public function getAuthIdentifier() { return 1; }\n}\n",
        ),
    ]
}

fn index() -> (tempfile::TempDir, CodeGraph) {
    let root = tempfile::tempdir().unwrap();
    write_files(root.path(), &files());
    let cg = CodeGraph::init_and_index(root.path().to_str().unwrap(), None).unwrap();
    (root, cg)
}

fn calls_of(cg: &CodeGraph, caller: &str, reference: Option<&str>) -> Vec<String> {
    let nodes = cg.store().get_all_nodes().unwrap();
    let source = nodes
        .iter()
        .find(|n| n.qualified_name == caller && matches!(n.kind, NodeKind::Method | NodeKind::Function))
        .unwrap_or_else(|| panic!("missing {caller}"));
    let mut out = Vec::new();
    for edge in cg.get_outgoing_edges(&source.id).unwrap() {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        if let Some(want) = reference {
            let got = edge.metadata.as_ref().and_then(|m| m.get("refName")).and_then(|v| v.as_str());
            if got != Some(want) {
                continue;
            }
        }
        let Some(target) = cg.get_node(&edge.target).unwrap() else { continue };
        out.push(target.qualified_name.clone());
    }
    out.sort();
    out
}

#[test]
fn a_receiver_the_file_cannot_name_does_not_walk_a_namesake() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, r"Drupal\comment\Controller::CommentController::permalink", Some("entity.access")),
        vec!["Drupal\\Core\\Entity::ContentEntityBase::access".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Drupal\comment\Controller::CommentController::replyAccess", Some("entity.access")),
        vec!["Drupal\\Core\\Access::AccessibleInterface::access".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Drupal\comment\Controller::CommentController::typeOf", Some("entity.getEntityType")),
        vec!["Drupal\\Core\\Entity::EntityInterface::getEntityType".to_string()]
    );
    cg.close();
}

#[test]
fn the_class_the_file_names_comes_before_a_namesake() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, r"Illuminate\View\Compilers::BladeCompiler::compile", Some("view.render")),
        vec!["Illuminate\\View::View::render".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"App\Http::PageController::store", Some("page.persist")),
        vec!["App\\Concerns::Saves::persist".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Illuminate\Tests::PivotUser::foreignKey", Some("pivot.getForeignKey")),
        vec!["Illuminate\\Database\\Eloquent\\Relations\\Concerns::AsPivot::getForeignKey".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Illuminate\Tests::PivotUser::key", Some("pivot.getKey")),
        vec!["Illuminate\\Database\\Eloquent::Model::getKey".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Illuminate\Types\Model::Comments::firstId", Some("comments.pluck")),
        vec!["Illuminate\\Database\\Eloquent::Collection::pluck".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Drupal\system\Controller::SystemController::page", Some("theme.getName")),
        vec!["Drupal\\Core\\Extension::Extension::getName".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"BookStack\Entities\Controllers::ChapterController::show", Some("chapter.getShortName")),
        vec!["BookStack\\Entities\\Models::Entity::getShortName".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Illuminate\Tests\Broadcasting::BroadcasterTest::testChannel", Some("user.getAuthIdentifier")),
        vec!["Illuminate\\Tests\\Broadcasting::GenericBroadcastingTestUser::getAuthIdentifier".to_string()]
    );
    cg.close();
}

#[test]
fn a_class_under_attributes_extends_the_parent_its_head_names() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, r"Drupal\taxonomy\Hook::TaxonomyHooks::deleted", Some("term.id")),
        vec!["Drupal\\Core\\Entity::EntityBase::id".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Drupal\taxonomy\Entity::Term::ownId", None),
        vec!["Drupal\\Core\\Entity::EntityBase::id".to_string()]
    );
    assert_eq!(calls_of(&cg, r"App\Providers::AppProvider::register", None), Vec::<String>::new());
    assert_eq!(
        calls_of(&cg, r"App\Providers::AppProvider::start", None),
        vec!["App\\Concerns::BootsTrait::boot".to_string()]
    );
    cg.close();
}

#[test]
fn a_facade_hands_the_call_on_and_a_factory_types_the_local() {
    let (_root, cg) = index();
    assert_eq!(
        calls_of(&cg, r"Illuminate\Support\Facades::Storage::fake", Some("ParallelTesting.token")),
        vec!["Illuminate\\Testing::ParallelTesting::token".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"Illuminate\Support\Facades::Storage::fake", Some("Cache.flushEverything")),
        Vec::<String>::new()
    );
    assert_eq!(
        calls_of(&cg, r"App\Models::PostTest::testStringKey", Some("post.tags")),
        vec!["App\\Models::PostStringPrimaryKey::tags".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"App\Models::PostTest::testNumericKey", Some("post.tags")),
        vec!["App\\Models::Post::tags".to_string()]
    );
    assert_eq!(
        calls_of(&cg, r"App\Models::PostTest::testOtherClass", Some("post.tags")),
        vec!["App\\Models::Post::tags".to_string()]
    );
    cg.close();
}
