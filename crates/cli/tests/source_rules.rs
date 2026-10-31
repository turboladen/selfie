// Rules the CLI's source keeps that no type can state, checked by parsing each
// file under `src` with syn and walking its tokens.
//
// Code under a `cfg` that requires `test` is removed first, so a test may
// format paths into fixtures by the hundred. Every other item's tokens are
// walked, a macro's arguments included. A string literal is one token and a
// comment is none, so neither can hide a call or fake one. A clippy ban does
// not fit these rules: it would fire on the tests, and an `#[expect]` over a
// test module silences the CLI's other method bans along with it.

use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::punctuated::Punctuated;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Meta, Token};

// A rule's name, and what to do instead, shown when the rule finds something.
struct Rule {
    name: &'static str,
    remedy: &'static str,
}

const DISPLAY: Rule = Rule {
    name: "a path's text is taken for display only in DisplayPath",
    remedy: "render these paths with DisplayPath::of or shorten_path",
};
const HANG: Rule = Rule {
    name: "only the layout modules indent continuation lines",
    remedy: "print these through a layout, such as DisplayManager::println_after",
};
const HIDDEN: Rule = Rule {
    name: "the hidden constructors are called only in terminal_text.rs",
    remedy: "build these with shown! or lit!",
};
const TABLE: Rule = Rule {
    name: "only tables.rs builds a comfy-table table",
    remedy: "build these tables with tables::SafeTable",
};

// The modules that place text on the screen, and the one that defines
// `Shown::hang`.
const LAYOUT_MODULES: &[&str] = &[
    "terminal_text.rs",
    "display_manager.rs",
    "tables.rs",
    "entry_rows.rs",
];

// A finding a rule allows, as (rule, file, context). Each must match at least
// once, so one that goes stale fails, and so does a strip pass that removes
// the code it names.
const EXEMPT: &[(&str, &str, &str)] = &[
    // `DisplayPath::of`, the one place a path's text is taken for display.
    (DISPLAY.name, "terminal_text.rs", "DisplayPath::of"),
];

// One thing a rule found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    rule: &'static str,
    file: String,
    line: usize,
    // The enclosing `Type::fn`, `fn` or item, as exemptions name it.
    context: String,
}

// One parsed source file.
struct Source {
    // The path under `src`, with `/` separators.
    file: String,
    whole: syn::File,
    // `whole` without the code that only tests compile.
    shipped: syn::File,
    // The out-of-line modules declared under a test-only `cfg`.
    out_of_line_tests: Vec<String>,
}

impl Source {
    fn parse(file: &str, text: &str) -> Self {
        let whole = syn::parse_file(text).unwrap_or_else(|error| panic!("{file}: {error}"));
        let mut shipped = whole.clone();
        let mut strip = Strip::default();
        if is_test_only(&shipped.attrs) {
            shipped.items.clear();
        } else {
            strip.visit_file_mut(&mut shipped);
        }
        Self {
            file: file.to_string(),
            whole,
            shipped,
            out_of_line_tests: strip.out_of_line,
        }
    }

    fn in_any(&self, modules: &[&str]) -> bool {
        modules.contains(&self.file.as_str())
    }
}

// Whether `attrs` hold a `cfg` that requires `test`: its predicate is `test`,
// or an `all(..)` with a member that requires it. `any(test, x)` and
// `not(test)` do not, so code under them stays checked.
fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<Meta>().is_ok_and(|m| requires_test(&m))
    })
}

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|members| members.iter().any(requires_test)),
        _ => false,
    }
}

// The outer attributes a node starts with, read off its tokens, so one
// function serves an item, an impl item, a trait item and a statement alike.
fn outer_attrs(node: &impl ToTokens) -> Vec<Attribute> {
    struct Outer(Vec<Attribute>);
    impl syn::parse::Parse for Outer {
        fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
            let attrs = input.call(Attribute::parse_outer)?;
            input.parse::<TokenStream>()?;
            Ok(Self(attrs))
        }
    }
    syn::parse2::<Outer>(node.to_token_stream())
        .map(|outer| outer.0)
        .unwrap_or_default()
}

fn test_only(node: &impl ToTokens) -> bool {
    is_test_only(&outer_attrs(node))
}

// Removes every item, impl item, trait item and statement that only tests
// compile, and notes each out-of-line test module, whose file carries no
// attribute of its own and would be walked as shipped code.
#[derive(Default)]
struct Strip {
    out_of_line: Vec<String>,
}

impl Strip {
    fn items(&mut self, items: &mut Vec<syn::Item>) {
        for item in items.iter() {
            if let syn::Item::Mod(module) = item
                && module.content.is_none()
                && test_only(item)
            {
                self.out_of_line.push(module.ident.to_string());
            }
        }
        items.retain(|item| !test_only(item));
    }
}

impl VisitMut for Strip {
    fn visit_file_mut(&mut self, file: &mut syn::File) {
        self.items(&mut file.items);
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut module.content {
            self.items(items);
        }
        visit_mut::visit_item_mod_mut(self, module);
    }

    fn visit_item_impl_mut(&mut self, block: &mut syn::ItemImpl) {
        block.items.retain(|item| !test_only(item));
        visit_mut::visit_item_impl_mut(self, block);
    }

    fn visit_item_trait_mut(&mut self, block: &mut syn::ItemTrait) {
        block.items.retain(|item| !test_only(item));
        visit_mut::visit_item_trait_mut(self, block);
    }

    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        block.stmts.retain(|stmt| !test_only(stmt));
        visit_mut::visit_block_mut(self, block);
    }
}

// A piece of a file the rules walk: an item, or one function of an impl or
// trait, with the context an exemption names it by.
struct Unit {
    context: String,
    tokens: TokenStream,
}

fn units(file: &syn::File) -> Vec<Unit> {
    let mut units = Vec::new();
    for item in &file.items {
        item_units(item, &mut units);
    }
    units
}

fn item_units(item: &syn::Item, units: &mut Vec<Unit>) {
    let mut push = |context: String, tokens: TokenStream| units.push(Unit { context, tokens });
    match item {
        syn::Item::Mod(module) => {
            if let Some((_, items)) = &module.content {
                for item in items {
                    item_units(item, units);
                }
            } else {
                push(module.ident.to_string(), item.to_token_stream());
            }
        }
        syn::Item::Impl(block) => {
            let owner = last_segment(&block.self_ty);
            for member in &block.items {
                let context = match member {
                    syn::ImplItem::Fn(function) => format!("{owner}::{}", function.sig.ident),
                    _ => owner.clone(),
                };
                push(context, member.to_token_stream());
            }
        }
        syn::Item::Trait(block) => {
            for member in &block.items {
                let context = match member {
                    syn::TraitItem::Fn(function) => {
                        format!("{}::{}", block.ident, function.sig.ident)
                    }
                    _ => block.ident.to_string(),
                };
                push(context, member.to_token_stream());
            }
        }
        syn::Item::Fn(function) => push(function.sig.ident.to_string(), item.to_token_stream()),
        other => push(item_name(other), other.to_token_stream()),
    }
}

// The last segment of `ty`'s path, `Choice` for `Choice<T>`; its tokens
// otherwise.
fn last_segment(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default(),
        other => other.to_token_stream().to_string(),
    }
}

fn item_name(item: &syn::Item) -> String {
    match item {
        syn::Item::Const(item) => item.ident.to_string(),
        syn::Item::Static(item) => item.ident.to_string(),
        syn::Item::Struct(item) => item.ident.to_string(),
        syn::Item::Enum(item) => item.ident.to_string(),
        syn::Item::Type(item) => item.ident.to_string(),
        syn::Item::Use(_) => "use".to_string(),
        syn::Item::Macro(item) => item
            .ident
            .as_ref()
            .map_or_else(|| "macro".to_string(), ToString::to_string),
        _ => String::new(),
    }
}

// Calls `visit` with each level of `tokens`, a group's contents included, and
// the index of each tree in it.
fn walk(tokens: TokenStream, visit: &mut impl FnMut(&[TokenTree], usize)) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    for index in 0..trees.len() {
        visit(&trees, index);
        if let TokenTree::Group(group) = &trees[index] {
            walk(group.stream(), visit);
        }
    }
}

fn ident_is(tree: Option<&TokenTree>, name: &str) -> bool {
    matches!(tree, Some(TokenTree::Ident(ident)) if ident.unraw() == name)
}

fn punct_is(tree: Option<&TokenTree>, c: char) -> bool {
    matches!(tree, Some(TokenTree::Punct(punct)) if punct.as_char() == c)
}

fn is_parenthesized(tree: Option<&TokenTree>) -> bool {
    matches!(tree, Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
}

// Whether the tree before `index` ends a `::`.
fn after_path_separator(trees: &[TokenTree], index: usize) -> bool {
    index >= 2
        && matches!(&trees[index - 1], TokenTree::Punct(p) if p.as_char() == ':')
        && matches!(&trees[index - 2], TokenTree::Punct(p) if p.as_char() == ':' && p.spacing() == Spacing::Joint)
}

// `.name(..)`, a method call.
fn method_call(trees: &[TokenTree], index: usize, name: &str) -> bool {
    ident_is(trees.get(index), name)
        && index > 0
        && punct_is(trees.get(index - 1), '.')
        && is_parenthesized(trees.get(index + 1))
}

// A finding for every token `matches` picks in `source`'s units.
fn find(
    rule: &'static Rule,
    source: &Source,
    units: &[Unit],
    matches: impl Fn(&[TokenTree], usize) -> bool,
) -> Vec<Finding> {
    let mut found = Vec::new();
    for unit in units {
        walk(unit.tokens.clone(), &mut |trees, index| {
            if matches(trees, index) {
                found.push(Finding {
                    rule: rule.name,
                    file: source.file.clone(),
                    line: trees[index].span().start().line,
                    context: unit.context.clone(),
                });
            }
        });
    }
    found
}

// Every rule's findings in `source`, exempt ones included.
fn findings(source: &Source) -> Vec<Finding> {
    let shipped = units(&source.shipped);
    let mut found = Vec::new();

    // `.display()`, and `Path::display` named as a function.
    found.extend(find(&DISPLAY, source, &shipped, |trees, index| {
        method_call(trees, index, "display")
            || (ident_is(trees.get(index), "display")
                && after_path_separator(trees, index)
                && !punct_is(trees.get(index + 1), ':'))
    }));

    if !source.in_any(LAYOUT_MODULES) {
        found.extend(find(&HANG, source, &shipped, |trees, index| {
            method_call(trees, index, "hang")
        }));
    }

    // Test code included: a test has no more business skipping the checks
    // `shown!` and `lit!` make than shipped code does.
    if source.file != "terminal_text.rs" {
        found.extend(find(
            &HIDDEN,
            source,
            &units(&source.whole),
            |trees, index| {
                ident_is(trees.get(index), "__assembled")
                    || ident_is(trees.get(index), "__from_literal")
            },
        ));
    }

    if source.file != "tables.rs" {
        found.extend(find(&TABLE, source, &shipped, |trees, index| {
            ident_is(trees.get(index), "comfy_table")
        }));
    }

    found
}

const RULES: &[&Rule] = &[&DISPLAY, &HANG, &HIDDEN, &TABLE];

fn sources() -> Vec<Source> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    assert!(files.len() > 20, "the walk found too few files: {files:?}");
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("commands/package/check.rs")),
        "the walk did not reach nested modules: {files:?}"
    );
    assert!(
        LAYOUT_MODULES
            .iter()
            .all(|module| files.contains(&src.join(module))),
        "a layout module moved: {files:?}"
    );
    files
        .iter()
        .map(|path| {
            let file = path
                .strip_prefix(&src)
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            Source::parse(&file, &std::fs::read_to_string(path).unwrap())
        })
        .collect()
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn the_cli_source_keeps_its_rules() {
    let sources = sources();
    let mut found: Vec<Finding> = sources.iter().flat_map(findings).collect();
    let out_of_line: Vec<String> = sources
        .iter()
        .flat_map(|source| {
            source
                .out_of_line_tests
                .iter()
                .map(move |module| format!("{}: mod {module};", source.file))
        })
        .collect();

    let mut unused = Vec::new();
    for (rule, file, context) in EXEMPT {
        let before = found.len();
        found.retain(|f| !(f.rule == *rule && f.file == *file && f.context == *context));
        if found.len() == before {
            unused.push(format!("({rule}, {file}, {context})"));
        }
    }

    let mut report = Vec::new();
    for rule in RULES {
        let hits: Vec<String> = found
            .iter()
            .filter(|f| f.rule == rule.name)
            .map(|f| format!("  src/{}:{} in {}", f.file, f.line, f.context))
            .collect();
        if !hits.is_empty() {
            report.push(format!(
                "{}; {}:\n{}",
                rule.name,
                rule.remedy,
                hits.join("\n")
            ));
        }
    }
    if !out_of_line.is_empty() {
        report.push(format!(
            "a test module declared out of line would be walked as shipped code; declare it \
             inline:\n  {}",
            out_of_line.join("\n  ")
        ));
    }
    if !unused.is_empty() {
        report.push(format!(
            "these exemptions matched nothing, so the code they name moved or the walk no \
             longer sees it:\n  {}",
            unused.join("\n  ")
        ));
    }
    assert!(report.is_empty(), "{}", report.join("\n\n"));
}

// What the rules find in `text`, as (rule, line), for a file at `file`.
fn found_in(file: &str, text: &str) -> Vec<(&'static str, usize)> {
    findings(&Source::parse(file, text))
        .into_iter()
        .map(|f| (f.rule, f.line))
        .collect()
}

// The walk sees `.display()` inside a macro's arguments and `Path::display`
// named as a function, and does not see one in a string, a comment, a test
// module or a test-only function in an impl. `any(test, ..)` code is checked.
#[test]
fn the_display_rule_sees_a_call_and_nothing_else() {
    let text = r#"
fn a(p: &Path) -> Shown { shown!("{}", p.display()) }
fn b(ps: &[&Path]) { ps.iter().map(Path::display); }
fn c() -> &'static str { ".display()" } // .display()
#[cfg(test)]
mod tests { fn d(p: &Path) { p.display(); } }
impl X {
    #[cfg(all(test, unix))]
    fn e(p: &Path) { p.display(); }
    #[cfg(any(test, unix))]
    fn f(p: &Path) { p.display(); }
}
fn g(processor: &P) { let _ = processor.display; }
"#;

    assert_eq!(
        found_in("commands/x.rs", text),
        [(DISPLAY.name, 2), (DISPLAY.name, 3), (DISPLAY.name, 11)]
    );
}

// A test-only statement inside a shipped function is not walked.
#[test]
fn a_test_only_statement_is_not_walked() {
    let text = "fn a(p: &Path) {\n    #[cfg(test)]\n    p.display();\n    p.display();\n}\n";

    assert_eq!(found_in("commands/x.rs", text), [(DISPLAY.name, 4)]);
}

// `.hang(` is seen chained over a line break, and only outside the layout
// modules.
#[test]
fn the_hang_rule_sees_a_call_outside_the_layout() {
    let text = "fn a(s: Shown) -> Shown { s.hang(2) }\n\
                fn b(s: Shown) -> Shown { s.paint(true, red)\n    .hang(4) }\n\
                fn c(s: Shown) -> Shown { s.change(2) }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(HANG.name, 1), (HANG.name, 3)]
    );
    assert_eq!(found_in("entry_rows.rs", text), []);
}

// A hidden constructor is seen called directly, as a raw identifier, and in
// test code; a comment or string naming one is not a call.
#[test]
fn the_hidden_constructor_rule_sees_every_call() {
    let text = "fn a() -> Shown { Shown::__assembled(String::new()) }\n\
                fn b() -> L { L::r#__from_literal(leaked()) }\n\
                // __assembled\n\
                fn c() -> &'static str { \"__from_literal\" }\n\
                #[cfg(test)]\nmod tests { fn d() { Shown::__assembled(x()); } }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(HIDDEN.name, 1), (HIDDEN.name, 2), (HIDDEN.name, 6)]
    );
    assert_eq!(found_in("terminal_text.rs", text), []);
}

// comfy_table is seen in an import outside tables.rs.
#[test]
fn the_table_rule_sees_comfy_table_outside_tables() {
    let text = "use comfy_table::Table;\nfn a() { let _ = comfy_table::Table::new(); }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(TABLE.name, 1), (TABLE.name, 2)]
    );
    assert_eq!(found_in("tables.rs", text), []);
}

// An out-of-line test module is noted, so the test can refuse it, and an
// inline one is not.
#[test]
fn an_out_of_line_test_module_is_noted() {
    let text = "#[cfg(test)]\nmod outside;\n#[cfg(test)]\nmod inside {}\nmod shipped;\n";

    assert_eq!(Source::parse("x.rs", text).out_of_line_tests, ["outside"]);
}
