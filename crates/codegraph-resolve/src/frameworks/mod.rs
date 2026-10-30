//! Framework resolvers — port of `src/resolution/frameworks/*`.

pub mod angular_router;
pub mod astro;
pub mod cargo_workspace;
pub mod cics;
pub mod component_folder;
pub mod csharp;
pub mod drupal;
pub mod expo_router;
pub mod expo_modules;
pub mod express;
pub mod fabric;
pub mod go;
pub mod goframe;
pub mod nextjs;
pub mod java;
pub mod laravel;
pub mod name_heuristic;
pub mod nestjs;
pub mod object_literal;
pub mod package_deps;
pub mod page_component;
pub mod play;
pub mod posix;
pub mod python;
pub mod react;
pub mod react_native;
pub mod react_router;
pub mod ruby;
pub mod rust;
pub mod svelte;
pub mod swift;
pub mod swift_objc;
pub mod sveltekit_router;
pub mod tanstack_router;
pub mod terraform;
pub mod vue;
pub mod vue_router;

use crate::ctx::Ctx;
use crate::types::FrameworkResolver;
use codegraph_types::Language;
use std::rc::Rc;

pub type FrameworkRef = Rc<dyn FrameworkResolver>;

/// Every framework resolver, in the order `frameworks/index.ts` registers them (the order
/// decides which resolver's candidate wins a confidence tie). Resolvers land here as their
/// TypeScript sources are ported.
pub fn all_framework_resolvers() -> Vec<FrameworkRef> {
    vec![
        Rc::new(laravel::LaravelResolver),
        Rc::new(drupal::DrupalResolver),
        Rc::new(express::ExpressResolver),
        Rc::new(nestjs::NestjsResolver),
        Rc::new(react::ReactResolver),
        Rc::new(react_router::ReactRouterResolver::new()),
        Rc::new(tanstack_router::TanstackRouterResolver::new()),
        Rc::new(nextjs::NextjsResolver::new()),
        Rc::new(svelte::SvelteResolver),
        Rc::new(sveltekit_router::SvelteKitRouterResolver::new()),
        Rc::new(vue::VueResolver),
        Rc::new(vue::NuxtResolver),
        Rc::new(vue_router::VueRouterResolver::new()),
        Rc::new(angular_router::AngularRouterResolver::new()),
        Rc::new(astro::AstroResolver),
        Rc::new(python::DjangoResolver),
        Rc::new(python::FlaskResolver),
        Rc::new(python::FastapiResolver),
        Rc::new(ruby::RailsResolver),
        Rc::new(java::SpringResolver),
        Rc::new(play::PlayResolver),
        Rc::new(go::GoResolver),
        Rc::new(goframe::GoframeResolver),
        Rc::new(rust::RustResolver),
        Rc::new(csharp::AspnetResolver),
        Rc::new(swift::SwiftUiResolver),
        Rc::new(swift::UikitResolver),
        Rc::new(swift::VaporResolver),
        Rc::new(swift_objc::SwiftObjcBridgeResolver),
        Rc::new(react_native::ReactNativeBridgeResolver),
        Rc::new(expo_modules::ExpoModulesResolver),
        Rc::new(expo_modules::ExpoModulesJsResolver),
        Rc::new(expo_router::ExpoRouterResolver::new()),
        Rc::new(fabric::FabricViewResolver),
        Rc::new(cics::CicsResolver),
        Rc::new(terraform::TerraformResolver),
    ]
}

/// Does the ROOT `package.json` declare one of `keys` in `dependencies` / `devDependencies`? The
/// `{ ...pkg.dependencies, ...pkg.devDependencies }` check several detectors open with: a
/// devDependency wins a clash, a falsy version (`""`) does not count, and an unreadable or
/// non-object manifest declares nothing.
pub(crate) fn root_manifest_declares(ctx: &Ctx, keys: &[&str]) -> bool {
    use serde_json::Value;
    let Some(package_json) = ctx.read_file("package.json") else { return false };
    let Ok(Value::Object(pkg)) = serde_json::from_str::<Value>(&package_json) else { return false };
    let truthy = |value: &Value| match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    };
    keys.iter().any(|key| {
        [pkg.get("devDependencies"), pkg.get("dependencies")]
            .into_iter()
            .flatten()
            .find_map(|deps| deps.as_object().and_then(|o| o.get(*key)))
            .is_some_and(truthy)
    })
}

/// Milliseconds since the epoch, for the `updated_at` of nodes a resolver extracts.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The resolvers whose `detect` accepts this project.
pub fn detect_frameworks(ctx: &Ctx) -> Vec<FrameworkRef> {
    all_framework_resolvers()
        .into_iter()
        .filter(|resolver| {
            // A throwing detector counts as "not detected".
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolver.detect(ctx))).unwrap_or(false)
        })
        .collect()
}

/// The detected resolvers whose `extract` runs on files in `language`.
pub fn get_applicable_frameworks(detected: &[FrameworkRef], language: Language) -> Vec<FrameworkRef> {
    detected
        .iter()
        .filter(|fw| fw.languages().is_none_or(|langs| langs.contains(&language)))
        .cloned()
        .collect()
}

/// The detected resolvers whose `resolve` sees references written in `language`.
pub fn get_resolving_frameworks(detected: &[FrameworkRef], language: Language) -> Vec<FrameworkRef> {
    detected
        .iter()
        .filter(|fw| {
            let languages = fw.resolve_languages().or_else(|| fw.languages());
            languages.is_none_or(|langs| langs.contains(&language))
        })
        .cloned()
        .collect()
}
