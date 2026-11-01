// These are rules the CLI's source keeps that no type can state. The test
// checks them by parsing each file under `src` with syn and walking its tokens.
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
use syn::visit::{self, Visit};
use syn::visit_mut::{self as visit_mut_fns, VisitMut};
use syn::{Attribute, Expr, Meta, Token};

// A rule has a name and a remedy, shown when it finds something, and the
// files it does not apply to. Each of those files must exist.
struct Rule {
    name: &'static str,
    remedy: &'static str,
    allowed_in: &'static [&'static str],
}

const DISPLAY: Rule = Rule {
    name: "a path's text is taken for display only in DisplayPath",
    remedy: "render these paths with DisplayPath::of or shorten_path",
    allowed_in: &[],
};
const HANG: Rule = Rule {
    name: "only the layout modules indent continuation lines",
    remedy: "print these through a layout, such as DisplayManager::println_after",
    allowed_in: LAYOUT_MODULES,
};
const HIDDEN: Rule = Rule {
    name: "the hidden constructors are called only in terminal_text.rs",
    remedy: "build these with shown! or lit!",
    allowed_in: &["terminal_text.rs"],
};
const TABLE: Rule = Rule {
    name: "only tables.rs builds a comfy-table table",
    remedy: "build these tables with tables::SafeTable",
    allowed_in: &["tables.rs"],
};
const INDENT: Rule = Rule {
    name: "no lit! or shown! literal indents the line after a line break",
    remedy: "print the continuation through a layout, which indents it at the column its text starts",
    allowed_in: LAYOUT_MODULES,
};
const COMPOSED: Rule = Rule {
    name: "a kind is built from one value, never from a composed string",
    remedy: "compose with shown! over kinds instead of formatting a string into one",
    allowed_in: &["terminal_text.rs", "sentence.rs"],
};
const LAX: Rule = Rule {
    name: "a kind that escapes less than a name is built only where the CLI receives a value or lays one out",
    remedy: "map the library field to its kind in received.rs, or show the value as a Name",
    allowed_in: RECEIVING_MODULES,
};
const MESSAGE: Rule = Rule {
    name: "message() takes a field or accessor named as a sentence",
    remedy: "pass the library's message, suggestion, reason or note; a name or path is a Name or DisplayPath",
    allowed_in: &["sentence.rs"],
};
const PRINTER: Rule = Rule {
    name: "a printer, cell or choice takes no string and no Display",
    remedy: "take a kind, a Shown or impl Into<Shown>",
    allowed_in: &[],
};
const CONSOLE: Rule = Rule {
    name: "console's process-wide color state is read only where colors are decided, and not styled through",
    remedy: "style through Shown::painted and let DisplayManager decide per stream",
    allowed_in: &[],
};
// SETTER is kept apart from CONSOLE, which exempts the function that reads
// console's answer, so no exemption lets that function set the state it reads.
const SETTER: Rule = Rule {
    name: "nothing sets console's process-wide color state",
    remedy: "decide colors in DisplayManager::new and leave console's state alone",
    allowed_in: &[],
};
const STYLE: Rule = Rule {
    name: "only terminal_text.rs builds a console Style",
    remedy: "style through Shown::painted, whose Style is forced in terminal_text.rs",
    allowed_in: &["terminal_text.rs"],
};
const INDICATIF: Rule = Rule {
    name: "only display_manager.rs uses indicatif",
    remedy: "show a waiting step through DisplayManager::start_waiting",
    allowed_in: &["display_manager.rs"],
};
const HANDLE: Rule = Rule {
    name: "only display_manager.rs holds console's terminal handles",
    remedy: "ask DisplayManager, which probes and writes the terminal",
    allowed_in: &["display_manager.rs"],
};
const WRITER: Rule = Rule {
    name: "only DisplayManager's guarded writers write to a stream",
    remedy: "print through a DisplayManager printer",
    allowed_in: &[],
};

// These modules place text on the screen, and the first defines `Shown::hang`.
const LAYOUT_MODULES: &[&str] = &[
    "terminal_text.rs",
    "display_manager.rs",
    "tables.rs",
    "entry_rows.rs",
];

// Each entry is a finding a rule allows, as (rule, file, context). Each must
// match at least once, so one that goes stale fails, and so does a strip pass
// that removes the code it names.
const EXEMPT: &[(&str, &str, &str)] = &[
    // `DisplayPath::of` is the one place a path's text is taken for display.
    (DISPLAY.name, "terminal_text.rs", "DisplayPath::of"),
    // Content that is not UTF-8 is described in two aligned lines under the
    // sentence, as a block laid out with the sentence it belongs to.
    (
        INDENT.name,
        "commands/apply.rs",
        "InteractiveConflictResolver::reveal",
    ),
    // A secret conflict's summary is a block the library laid out, kept as
    // prose with its own indentation.
    (
        LAX.name,
        "commands/apply.rs",
        "InteractiveConflictResolver::resolve",
    ),
    // A choice's label is escaped as a Name inside it.
    (PRINTER.name, "display_manager.rs", "Choice::named"),
    // This is the one place console's answer for each stream is read.
    (CONSOLE.name, "display_manager.rs", "console_colors"),
    // These are the guarded writers, which the rule exists to keep alone.
    (WRITER.name, "display_manager.rs", "DisplayManager::write"),
    (
        WRITER.name,
        "display_manager.rs",
        "DisplayManager::put_above_spinners",
    ),
    (WRITER.name, "display_manager.rs", "VerboseLog::make_writer"),
    // The first signal's notice is written from the signal task, where no
    // display is in reach. It is selfie's own unstyled text.
    (WRITER.name, "main.rs", "until_force_quit"),
    // A completion script is for the shell to read, not text to show.
    (WRITER.name, "commands/completion.rs", "generate_completion"),
];

// Each kind is listed with the constructors that build it.
const KIND_CONSTRUCTORS: &[(&str, &[&str])] = &[
    ("Name", &["new"]),
    ("DisplayPath", &["new", "of"]),
    ("Prose", &["new"]),
    ("Output", &["new", "line"]),
    ("Content", &["new"]),
    ("Command", &["composed", "written", "one_line"]),
];

// These constructors build the kinds that escape less than the strict set.
const LAX_CONSTRUCTORS: &[(&str, &str)] = &[
    ("Prose", "new"),
    ("Output", "new"),
    ("Output", "line"),
    ("Content", "new"),
    ("Command", "written"),
];

// A lax kind may be built in these modules: the kinds' own module, the ones
// that map a library value to its kind (an event's fields where the event is
// handled), and the layouts that show file content.
const RECEIVING_MODULES: &[&str] = &[
    "terminal_text.rs",
    "sentence.rs",
    "received.rs",
    "event_processor.rs",
    "display_manager.rs",
    "snippet.rs",
];

// A sentence the library hands over as a string is read from one of these names.
const SENTENCE_FIELDS: &[&str] = &[
    "message",
    "suggestion",
    "reason",
    "note",
    "warning",
    "clause",
    "sentence",
    "claimants",
    "error",
    "refusal",
    "refusal_elsewhere",
    "hint",
];

// `message`'s argument may pass through these before the name it is read
// from: borrowing, and conversions that keep the text.
const ADAPTERS: &[&str] = &[
    "as_str",
    "as_deref",
    "as_ref",
    "clone",
    "to_string",
    "to_owned",
    "unwrap_or_default",
    "trim",
    "trim_start",
    "trim_end",
    "trim_start_matches",
    "trim_end_matches",
];

// These types' methods show text.
const PRINTER_TYPES: &[&str] = &[
    "DisplayManager",
    "ResultCard",
    "SafeTable",
    "Cell",
    "Choice",
];

// A printer's parameter type may not name any of these.
const TEXT_TYPES: &[&str] = &["str", "String", "Display", "ToString"];

// No CLI code calls these from console, outside the exemptions.
const CONSOLE_BANNED: &[&str] = &["style", "colors_enabled", "colors_enabled_stderr"];

// These set console's color state. Nothing is exempt.
const CONSOLE_SETTERS: &[&str] = &[
    "set_colors_enabled",
    "set_colors_enabled_stderr",
    "set_true_colors_enabled",
    "set_true_colors_enabled_stderr",
];

// A finding is one thing a rule found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    rule: &'static str,
    file: String,
    line: usize,
    // This names the enclosing `Type::fn`, `fn` or item, as exemptions do.
    context: String,
}

impl Finding {
    fn at(rule: &Rule, source: &Source, line: usize, context: &str) -> Self {
        Self {
            rule: rule.name,
            file: source.file.clone(),
            line,
            context: context.to_string(),
        }
    }
}

// A source is one parsed file.
struct Source {
    // This is the path under `src`, with `/` separators.
    file: String,
    whole: syn::File,
    // This is `whole` with the code that only tests compile removed.
    shipped: syn::File,
    // These are the out-of-line modules declared under a test-only `cfg`.
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

// A node that can carry outer attributes, read from its syn variant.
trait Attrs {
    fn attrs(&self) -> &[Attribute];
}

// Returns the `attrs` of whichever listed variant `$node` is, and none for a
// variant syn keeps as raw tokens.
macro_rules! attrs_of {
    ($node:expr, $kind:ident: $($variant:ident),+) => {
        match $node {
            $($kind::$variant(inner) => &inner.attrs,)+
            _ => &[],
        }
    };
}

impl Attrs for syn::Item {
    fn attrs(&self) -> &[Attribute] {
        use syn::Item;
        attrs_of!(self, Item: Const, Enum, ExternCrate, Fn, ForeignMod, Impl, Macro, Mod, Static,
            Struct, Trait, TraitAlias, Type, Union, Use)
    }
}

impl Attrs for syn::ImplItem {
    fn attrs(&self) -> &[Attribute] {
        use syn::ImplItem;
        attrs_of!(self, ImplItem: Const, Fn, Type, Macro)
    }
}

impl Attrs for syn::TraitItem {
    fn attrs(&self) -> &[Attribute] {
        use syn::TraitItem;
        attrs_of!(self, TraitItem: Const, Fn, Type, Macro)
    }
}

impl Attrs for syn::Expr {
    fn attrs(&self) -> &[Attribute] {
        attrs_of!(self, Expr: Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure,
            Const, Continue, Field, ForLoop, Group, If, Index, Infer, Let, Lit, Loop, Macro, Match,
            MethodCall, Paren, Path, Range, RawAddr, Reference, Repeat, Return, Struct, Try,
            TryBlock, Tuple, Unary, Unsafe, While, Yield)
    }
}

impl Attrs for syn::Stmt {
    fn attrs(&self) -> &[Attribute] {
        match self {
            syn::Stmt::Local(local) => &local.attrs,
            syn::Stmt::Item(item) => item.attrs(),
            syn::Stmt::Expr(expr, _) => expr.attrs(),
            syn::Stmt::Macro(mac) => &mac.attrs,
        }
    }
}

impl Attrs for syn::FnArg {
    fn attrs(&self) -> &[Attribute] {
        match self {
            syn::FnArg::Receiver(receiver) => &receiver.attrs,
            syn::FnArg::Typed(typed) => &typed.attrs,
        }
    }
}

fn test_only(node: &impl Attrs) -> bool {
    is_test_only(node.attrs())
}

// Removes every item, impl item, trait item, statement, struct field, field
// value, enum variant, match arm and function parameter that only tests
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
        visit_mut_fns::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut module.content {
            self.items(items);
        }
        visit_mut_fns::visit_item_mod_mut(self, module);
    }

    fn visit_item_impl_mut(&mut self, block: &mut syn::ItemImpl) {
        block.items.retain(|item| !test_only(item));
        visit_mut_fns::visit_item_impl_mut(self, block);
    }

    fn visit_item_trait_mut(&mut self, block: &mut syn::ItemTrait) {
        block.items.retain(|item| !test_only(item));
        visit_mut_fns::visit_item_trait_mut(self, block);
    }

    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        block.stmts.retain(|stmt| !test_only(stmt));
        visit_mut_fns::visit_block_mut(self, block);
    }

    fn visit_fields_named_mut(&mut self, fields: &mut syn::FieldsNamed) {
        fields.named = std::mem::take(&mut fields.named)
            .into_iter()
            .filter(|field| !is_test_only(&field.attrs))
            .collect();
        visit_mut_fns::visit_fields_named_mut(self, fields);
    }

    fn visit_expr_struct_mut(&mut self, value: &mut syn::ExprStruct) {
        value.fields = std::mem::take(&mut value.fields)
            .into_iter()
            .filter(|field| !is_test_only(&field.attrs))
            .collect();
        visit_mut_fns::visit_expr_struct_mut(self, value);
    }

    fn visit_item_enum_mut(&mut self, item: &mut syn::ItemEnum) {
        item.variants = std::mem::take(&mut item.variants)
            .into_iter()
            .filter(|variant| !is_test_only(&variant.attrs))
            .collect();
        visit_mut_fns::visit_item_enum_mut(self, item);
    }

    fn visit_expr_match_mut(&mut self, value: &mut syn::ExprMatch) {
        value.arms.retain(|arm| !is_test_only(&arm.attrs));
        visit_mut_fns::visit_expr_match_mut(self, value);
    }

    fn visit_signature_mut(&mut self, signature: &mut syn::Signature) {
        signature.inputs = std::mem::take(&mut signature.inputs)
            .into_iter()
            .filter(|input| !test_only(input))
            .collect();
        visit_mut_fns::visit_signature_mut(self, signature);
    }
}

// A unit is a piece of a file the rules walk: an item, or one function of an
// impl or trait, with the context an exemption names it by.
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

// Returns the last segment of `ty`'s path, `Choice` for `Choice<T>`, or its
// tokens for a type that is not a path.
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

// Whether the method call `trees[index]` names is made on `self` or
// `self.display`, the receivers a DisplayManager printer is called on in
// display_manager.rs.
fn called_on_the_display(trees: &[TokenTree], index: usize) -> bool {
    // No expression puts a `.` before `self`, so `self` here starts the chain.
    let on_self = index >= 2 && ident_is(trees.get(index - 2), "self");
    let on_display = index >= 4
        && ident_is(trees.get(index - 2), "display")
        && punct_is(trees.get(index - 3), '.')
        && ident_is(trees.get(index - 4), "self");
    on_self || on_display
}

// Whether `trees[index]` names a method call, `.name(..)`.
fn method_call(trees: &[TokenTree], index: usize, name: &str) -> bool {
    ident_is(trees.get(index), name)
        && index > 0
        && punct_is(trees.get(index - 1), '.')
        && is_parenthesized(trees.get(index + 1))
}

// Returns a finding for every token `matches` picks in `source`'s units,
// unless the rule does not apply to the file.
fn find<'a>(
    rule: &'static Rule,
    source: &Source,
    units: impl IntoIterator<Item = &'a Unit>,
    matches: impl Fn(&[TokenTree], usize) -> bool,
) -> Vec<Finding> {
    let mut found = Vec::new();
    if source.in_any(rule.allowed_in) {
        return found;
    }
    for unit in units {
        walk(unit.tokens.clone(), &mut |trees, index| {
            if matches(trees, index) {
                let line = trees[index].span().start().line;
                found.push(Finding::at(rule, source, line, &unit.context));
            }
        });
    }
    found
}

// Returns every rule's findings in `source`, exempt ones included.
fn findings(source: &Source) -> Vec<Finding> {
    let shipped = units(&source.shipped);
    let imports = imports(&source.shipped);
    // A `use` is judged by its path, not by its tokens again.
    let code: Vec<&Unit> = shipped
        .iter()
        .filter(|unit| unit.context != "use")
        .collect();
    let mut found = Vec::new();

    // This finds `.display()`, and `Path::display` named as a function.
    found.extend(find(&DISPLAY, source, &shipped, |trees, index| {
        method_call(trees, index, "display")
            || (ident_is(trees.get(index), "display")
                && after_path_separator(trees, index)
                && !punct_is(trees.get(index + 1), ':'))
    }));
    found.extend(find(&HANG, source, &shipped, |trees, index| {
        method_call(trees, index, "hang") || ident_is(trees.get(index), "hang_text")
    }));
    // This walks test code too: a test has no more business skipping the
    // checks `shown!` and `lit!` make than shipped code does.
    found.extend(find(
        &HIDDEN,
        source,
        &units(&source.whole),
        |trees, index| {
            ident_is(trees.get(index), "__assembled")
                || ident_is(trees.get(index), "__from_literal")
        },
    ));
    found.extend(find(&TABLE, source, &shipped, |trees, index| {
        ident_is(trees.get(index), "comfy_table")
    }));
    found.extend(find(&INDICATIF, source, &shipped, |trees, index| {
        ident_is(trees.get(index), "indicatif")
    }));
    found.extend(find(&INDENT, source, &shipped, indented_literal));
    found.extend(find(&COMPOSED, source, &shipped, |trees, index| {
        kind_arguments(trees, index).is_some_and(composes)
    }));
    found.extend(find(&LAX, source, &shipped, |trees, index| {
        LAX_CONSTRUCTORS
            .iter()
            .any(|(kind, constructor)| constructor_call(trees, index, kind, constructor))
    }));
    found.extend(messages(source));
    found.extend(printers(source));

    let console = |path: &[String], names: &[&str]| {
        path.first().is_some_and(|first| first == "console")
            && path
                .last()
                .is_some_and(|last| names.contains(&last.as_str()))
    };
    let nothing_else = |_: &[TokenTree], _: usize| false;
    found.extend(paths(
        &CONSOLE,
        source,
        &imports,
        &code,
        |path| {
            console(path, CONSOLE_BANNED)
                || console(path, &["*"])
                || tail_is(path, "Term", &["style"])
        },
        |trees, index| {
            method_call(trees, index, "for_stderr") || method_call(trees, index, "for_stdout")
        },
    ));
    found.extend(paths(
        &SETTER,
        source,
        &imports,
        &code,
        |path| console(path, CONSOLE_SETTERS),
        nothing_else,
    ));
    found.extend(paths(
        &STYLE,
        source,
        &imports,
        &code,
        |path| tail_is(path, "Style", &["new", "default", "from_dotted_str"]),
        nothing_else,
    ));
    found.extend(paths(
        &WRITER,
        source,
        &imports,
        &code,
        |path| tail_is(path, "io", &["stdout", "stderr"]),
        |trees, index| {
            let print_macro = ["println", "eprintln", "print", "eprint"]
                .iter()
                .any(|name| ident_is(trees.get(index), name))
                && punct_is(trees.get(index + 1), '!');
            // Only display_manager.rs can hold indicatif's bars (INDICATIF), and
            // their `println` writes. There, a `.println(..)` on anything but
            // the display itself is one of theirs, whatever names it.
            let progress_write = source.file == "display_manager.rs"
                && method_call(trees, index, "println")
                && !called_on_the_display(trees, index);
            print_macro || progress_write
        },
    ));
    found.extend(paths(
        &HANDLE,
        source,
        &imports,
        &code,
        |path| tail_is(path, "Term", &["stdout", "stderr"]),
        nothing_else,
    ));

    found
}

const RULES: &[&Rule] = &[
    &DISPLAY, &HANG, &HIDDEN, &TABLE, &INDENT, &COMPOSED, &LAX, &MESSAGE, &PRINTER, &CONSOLE,
    &SETTER, &STYLE, &WRITER, &HANDLE, &INDICATIF,
];

// Whether `trees[index]` starts a `lit!` or `shown!` whose literal has a line
// break followed by a space or a tab. The literal is judged by its value, so a
// `\`-continued line is not one.
fn indented_literal(trees: &[TokenTree], index: usize) -> bool {
    if !(ident_is(trees.get(index), "lit") || ident_is(trees.get(index), "shown"))
        || !punct_is(trees.get(index + 1), '!')
    {
        return false;
    }
    let Some(TokenTree::Group(group)) = trees.get(index + 2) else {
        return false;
    };
    let Some(TokenTree::Literal(literal)) = group.stream().into_iter().next() else {
        return false;
    };
    syn::parse2::<syn::LitStr>(literal.to_token_stream())
        .is_ok_and(|text| text.value().contains("\n ") || text.value().contains("\n\t"))
}

// Whether `trees[index]` starts `kind::constructor(..)`.
fn constructor_call(trees: &[TokenTree], index: usize, kind: &str, constructor: &str) -> bool {
    ident_is(trees.get(index), kind)
        && punct_is(trees.get(index + 1), ':')
        && punct_is(trees.get(index + 2), ':')
        && ident_is(trees.get(index + 3), constructor)
        && is_parenthesized(trees.get(index + 4))
}

// Returns the arguments of the kind constructor, `sentence(..)` or
// `message(..)` that `trees[index]` starts.
fn kind_arguments(trees: &[TokenTree], index: usize) -> Option<TokenStream> {
    let group = |at: usize| match trees.get(at) {
        Some(TokenTree::Group(group)) => Some(group.stream()),
        _ => None,
    };
    for (kind, constructors) in KIND_CONSTRUCTORS {
        for constructor in *constructors {
            if constructor_call(trees, index, kind, constructor) {
                return group(index + 4);
            }
        }
    }
    let function = ident_is(trees.get(index), "sentence") || ident_is(trees.get(index), "message");
    let method = index > 0 && punct_is(trees.get(index - 1), '.');
    if function && !method && is_parenthesized(trees.get(index + 1)) {
        return group(index + 1);
    }
    None
}

// Whether `arguments` compose a string: a macro other than `lit!`, or a
// `.join(..)` or `.concat()`.
fn composes(arguments: TokenStream) -> bool {
    let mut composed = false;
    walk(arguments, &mut |trees, index| {
        let macro_call = matches!(trees.get(index), Some(TokenTree::Ident(ident)) if ident != "lit")
            && punct_is(trees.get(index + 1), '!')
            && matches!(trees.get(index + 2), Some(TokenTree::Group(_)));
        composed |=
            macro_call || method_call(trees, index, "join") || method_call(trees, index, "concat");
    });
    composed
}

// Returns the name an expression is read from, once the adapters are peeled:
// a field's member, a method's name, a function's last segment, or a path's.
fn read_from(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Reference(inner) => read_from(&inner.expr),
        Expr::Unary(inner) => read_from(&inner.expr),
        Expr::Paren(inner) => read_from(&inner.expr),
        Expr::MethodCall(call) if ADAPTERS.iter().any(|name| call.method == name) => {
            read_from(&call.receiver)
        }
        Expr::MethodCall(call) => Some(call.method.to_string()),
        Expr::Field(field) => Some(field.member.to_token_stream().to_string()),
        Expr::Call(call) => match &*call.func {
            Expr::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        },
        Expr::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn names_message(expr: &Expr) -> bool {
    matches!(expr, Expr::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "message"))
}

// Finds each `message(..)` call, and each `message` passed to `.map` and its
// kin, whose text is not read from a sentence-shaped name. A macro's body is
// checked when it parses as expressions.
fn messages(source: &Source) -> Vec<Finding> {
    struct Messages<'a> {
        source: &'a Source,
        found: Vec<Finding>,
    }
    impl Messages<'_> {
        fn judge(&mut self, read: &Expr, at: &impl syn::spanned::Spanned) {
            let named =
                read_from(read).is_some_and(|name| SENTENCE_FIELDS.contains(&name.as_str()));
            if !named {
                let line = at.span().start().line;
                self.found
                    .push(Finding::at(&MESSAGE, self.source, line, ""));
            }
        }
    }
    impl<'ast> Visit<'ast> for Messages<'_> {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if names_message(&call.func) {
                match call.args.first() {
                    Some(argument) => self.judge(argument, &call.func),
                    None => self.judge(&Expr::Verbatim(TokenStream::new()), &call.func),
                }
            }
            visit::visit_expr_call(self, call);
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if ["map", "and_then", "map_or", "map_or_else"]
                .iter()
                .any(|name| call.method == name)
                && call.args.iter().any(names_message)
            {
                self.judge(&call.receiver, &call.method);
            }
            visit::visit_expr_method_call(self, call);
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            if let Ok(arguments) =
                mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            {
                for argument in &arguments {
                    self.visit_expr(argument);
                }
            }
            visit::visit_macro(self, mac);
        }
    }
    if source.in_any(MESSAGE.allowed_in) {
        return Vec::new();
    }
    let mut messages = Messages {
        source,
        found: Vec::new(),
    };
    messages.visit_file(&source.shipped);
    messages.found
}

// Finds every method of a printer type that is not private and takes a
// parameter whose type names a string or `Display`, judging a generic
// parameter by its bounds.
fn printers(source: &Source) -> Vec<Finding> {
    struct Printers<'a> {
        source: &'a Source,
        found: Vec<Finding>,
    }
    impl<'ast> Visit<'ast> for Printers<'_> {
        fn visit_item_impl(&mut self, block: &'ast syn::ItemImpl) {
            let owner = last_segment(&block.self_ty);
            if PRINTER_TYPES.contains(&owner.as_str()) {
                for member in &block.items {
                    if let syn::ImplItem::Fn(function) = member
                        && !matches!(function.vis, syn::Visibility::Inherited)
                        && takes_text(&block.generics, &function.sig)
                    {
                        let line = function.sig.ident.span().start().line;
                        let context = format!("{owner}::{}", function.sig.ident);
                        self.found
                            .push(Finding::at(&PRINTER, self.source, line, &context));
                    }
                }
            }
            visit::visit_item_impl(self, block);
        }
    }
    let mut printers = Printers {
        source,
        found: Vec::new(),
    };
    printers.visit_file(&source.shipped);
    printers.found
}

fn names_text(tokens: TokenStream) -> bool {
    let mut named = false;
    walk(tokens, &mut |trees, index| {
        named |= TEXT_TYPES
            .iter()
            .any(|name| ident_is(trees.get(index), name));
    });
    named
}

// Whether a parameter of `signature` names a string or `Display`, itself or
// through the bounds of a generic it names, from the method or its impl.
fn takes_text(outer: &syn::Generics, signature: &syn::Signature) -> bool {
    let mut bounds: Vec<(String, TokenStream)> = Vec::new();
    for generics in [outer, &signature.generics] {
        for param in generics.type_params() {
            bounds.push((param.ident.to_string(), param.bounds.to_token_stream()));
        }
        if let Some(clause) = &generics.where_clause {
            for predicate in &clause.predicates {
                if let syn::WherePredicate::Type(predicate) = predicate {
                    bounds.push((
                        predicate.bounded_ty.to_token_stream().to_string(),
                        predicate.bounds.to_token_stream(),
                    ));
                }
            }
        }
    }
    signature.inputs.iter().any(|input| {
        let syn::FnArg::Typed(param) = input else {
            return false;
        };
        let ty = param.ty.to_token_stream();
        let mut through_generic = false;
        walk(ty.clone(), &mut |trees, index| {
            if let Some(TokenTree::Ident(ident)) = trees.get(index) {
                through_generic |= bounds
                    .iter()
                    .any(|(name, bound)| ident == name && names_text(bound.clone()));
            }
        });
        names_text(ty) || through_generic
    })
}

// Returns the path a token ends, `console::style` for `style` in
// `console::style(..)`.
fn path_ending(trees: &[TokenTree], index: usize) -> Vec<String> {
    let mut path = Vec::new();
    let mut at = index;
    while let Some(TokenTree::Ident(ident)) = trees.get(at) {
        path.push(ident.unraw().to_string());
        if at < 3 || !after_path_separator(trees, at) {
            break;
        }
        at -= 3;
    }
    path.reverse();
    path
}

// Returns every name a file's `use` items bring in, as (local name, full
// path, line), and every glob, as a path ending in `*`.
fn imports(file: &syn::File) -> Vec<Import> {
    fn flatten(prefix: &[String], tree: &syn::UseTree, line: usize, into: &mut Vec<Import>) {
        let with = |name: &syn::Ident| {
            let mut path = prefix.to_vec();
            path.push(name.unraw().to_string());
            path
        };
        match tree {
            syn::UseTree::Path(path) => flatten(&with(&path.ident), &path.tree, line, into),
            syn::UseTree::Name(name) => {
                into.push((name.ident.unraw().to_string(), with(&name.ident), line));
            }
            syn::UseTree::Rename(rename) => {
                into.push((rename.rename.unraw().to_string(), with(&rename.ident), line));
            }
            syn::UseTree::Glob(_) => {
                let mut path = prefix.to_vec();
                path.push("*".to_string());
                into.push(("*".to_string(), path, line));
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    flatten(prefix, item, line, into);
                }
            }
        }
    }
    struct Uses(Vec<Import>);
    impl<'ast> Visit<'ast> for Uses {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            flatten(
                &[],
                &item.tree,
                item.use_token.span.start().line,
                &mut self.0,
            );
        }
        fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
            if let Some((_, rename)) = &item.rename {
                self.0.push((
                    rename.unraw().to_string(),
                    vec![item.ident.unraw().to_string()],
                    item.ident.span().start().line,
                ));
            }
        }
    }
    let mut uses = Uses(Vec::new());
    uses.visit_file(file);
    uses.0
}

// An import is a name a `use` brings in: its local name, its full path, and
// its line.
type Import = (String, Vec<String>, usize);

// Returns `path` with its first segment resolved through `imports`.
fn resolved(path: &[String], imports: &[Import]) -> Vec<String> {
    let Some((first, rest)) = path.split_first() else {
        return Vec::new();
    };
    match imports.iter().find(|(local, _, _)| local == first) {
        Some((_, full, _)) => full.iter().chain(rest).cloned().collect(),
        None => path.to_vec(),
    }
}

// Whether `path` ends in `parent::child` for one of `children`.
fn tail_is(path: &[String], parent: &str, children: &[&str]) -> bool {
    matches!(path, [.., p, c] if p == parent && children.contains(&c.as_str()))
}

// Returns a finding for each import whose full path `judge` picks, each path
// in code it picks once resolved through the imports, and each token `also`
// picks.
fn paths(
    rule: &'static Rule,
    source: &Source,
    imports: &[Import],
    code: &[&Unit],
    judge: impl Fn(&[String]) -> bool,
    also: impl Fn(&[TokenTree], usize) -> bool,
) -> Vec<Finding> {
    if source.in_any(rule.allowed_in) {
        return Vec::new();
    }
    let mut found: Vec<Finding> = imports
        .iter()
        .filter(|(_, path, _)| judge(path))
        .map(|(_, _, line)| Finding::at(rule, source, *line, "use"))
        .collect();
    found.extend(find(rule, source, code.iter().copied(), |trees, index| {
        let ends_path =
            !(punct_is(trees.get(index + 1), ':') && punct_is(trees.get(index + 2), ':'));
        let path = resolved(&path_ending(trees, index), imports);
        (path.len() > 1 && ends_path && judge(&path)) || also(trees, index)
    }));
    found
}

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
    // A rule's allowed files and an exemption's file are named as text, so
    // each must still exist or the rule would quietly cover less.
    let named = RULES
        .iter()
        .flat_map(|rule| rule.allowed_in.iter())
        .chain(EXEMPT.iter().map(|(_, file, _)| file));
    for file in named {
        assert!(
            files.contains(&src.join(file)),
            "{file} is named by a rule but moved: {files:?}"
        );
    }
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

    let unused = remove_exempt(&mut found);

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

// Removes the exempt findings from `found`, and returns each exemption that
// matched none.
fn remove_exempt(found: &mut Vec<Finding>) -> Vec<String> {
    let mut unused = Vec::new();
    for (rule, file, context) in EXEMPT {
        let before = found.len();
        found.retain(|f| !(f.rule == *rule && f.file == *file && f.context == *context));
        if found.len() == before {
            unused.push(format!("({rule}, {file}, {context})"));
        }
    }
    unused
}

// Returns what the rules find in `text`, as (rule, line), for a file at
// `file`.
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

// A test-only struct field, field value, enum variant, match arm or
// parameter is not walked; the shipped ones beside each are.
#[test]
fn test_only_fields_variants_arms_and_parameters_are_not_walked() {
    let text = r#"
struct S {
    #[cfg(test)]
    t: [u8; { p().display(); 1 }],
    s: [u8; { p().display(); 1 }],
}
enum E {
    #[cfg(test)]
    T = { p().display(); 1 },
    S = { p().display(); 2 },
}
fn a(x: u8) -> S {
    match x {
        #[cfg(test)]
        0 => p().display(),
        _ => p().display(),
    };
    S {
        #[cfg(test)]
        t: p().display(),
        s: p().display(),
    }
}
fn b(#[cfg(test)] t: [u8; { p().display(); 1 }], s: [u8; { p().display(); 1 }]) {}
"#;

    let lines: Vec<usize> = found_in("commands/x.rs", text)
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    assert_eq!(lines, [5, 10, 16, 21, 24]);
}

// `.hang(` is seen chained over a line break, and `hang_text` called on its
// own, only outside the layout modules.
#[test]
fn the_hang_rule_sees_a_call_outside_the_layout() {
    let text = "fn a(s: Shown) -> Shown { s.hang(2) }\n\
                fn b(s: Shown) -> Shown { s.paint(true, red)\n    .hang(4) }\n\
                fn c(s: Shown) -> Shown { s.change(2) }\n\
                fn d(t: &str) -> String { terminal_text::hang_text(t, 2) }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(HANG.name, 1), (HANG.name, 3), (HANG.name, 5)]
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

// An indent after a line break is seen in a `lit!` or `shown!` literal by its
// value: a `\`-continued line has none, and a layout module may indent.
#[test]
fn the_indent_rule_sees_a_literal_that_indents() {
    let text = "fn a() -> Literal { lit!(\"a\\n  b\") }\n\
                fn b(x: Name) -> Shown { shown!(\"{}\\n\\t{}\", x, x) }\n\
                fn c() -> Literal { lit!(\"a \\\n    b\") }\n\
                fn d() -> Literal { lit!(\"a\\n\\nb\") }\n\
                fn e() -> String { format!(\"a\\n  b\") }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(INDENT.name, 1), (INDENT.name, 2)]
    );
    assert_eq!(found_in("tables.rs", text), []);
}

// A kind, `sentence` or `message` built from a composed string is seen; one
// built from a value, or from `lit!`, is not.
#[test]
fn the_composed_rule_sees_a_formatted_kind() {
    let text = "fn a(x: &str) -> Name { Name::new(&format!(\"{}\", x)) }\n\
                fn b(e: &E) -> Prose { sentence(format!(\"{e}\")) }\n\
                fn c(parts: &[&str]) -> Prose { message(parts.join(\", \")) }\n\
                fn d(x: &str) -> Name { Name::new(x) }\n\
                fn e() -> Option<Command> { Command::composed([Word::Literal(lit!(\"x\"))]) }\n\
                fn f(x: &str) -> std::process::Command { std::process::Command::new(format!(\"{x}\")) }\n";

    let found: Vec<_> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == COMPOSED.name)
        .collect();
    assert_eq!(
        found,
        [(COMPOSED.name, 1), (COMPOSED.name, 2), (COMPOSED.name, 3)]
    );
}

// A lax kind built in a command module is seen; the same in a receiving
// module, or a strict kind anywhere, is not.
#[test]
fn the_lax_rule_sees_a_lax_kind_built_by_a_handler() {
    let text = "fn a(x: &str) -> Prose { Prose::new(x) }\n\
                fn b(x: &str) -> Command { Command::written(x) }\n\
                fn c(x: &str) -> Name { Name::new(x) }\n\
                fn d(x: &str) -> std::process::Command { std::process::Command::new(x) }\n";

    let found: Vec<_> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == LAX.name)
        .collect();
    assert_eq!(found, [(LAX.name, 1), (LAX.name, 2)]);
    assert_eq!(found_in("received.rs", text), []);
}

// `message` takes text read from a sentence-shaped name, through adapters,
// also as `.map(message)` and inside a macro; a name, or text read from
// nothing, is seen.
#[test]
fn the_message_rule_sees_text_not_read_from_a_sentence() {
    let text = "fn a(r: &R) -> Prose { message(r.message().as_str()) }\n\
                fn b(d: &D) -> Option<Prose> { d.refusal.as_deref().map(message) }\n\
                fn c(name: &str) -> Prose { message(name) }\n\
                fn d(d: &D) -> Option<Prose> { d.name.as_deref().map(message) }\n\
                fn e() -> Shown { shown!(\"{}\", message(package_name)) }\n\
                fn f() -> Prose { message(\"x\") }\n\
                fn g(w: &W) -> Prose { crate::sentence::message(&w.warning) }\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [
            (MESSAGE.name, 3),
            (MESSAGE.name, 4),
            (MESSAGE.name, 5),
            (MESSAGE.name, 6)
        ]
    );
}

// A printer method taking text is seen at any depth of its type, through a
// generic's bounds, and in an impl nested in a module; a private one, and one
// taking a kind, are not.
#[test]
fn the_printer_rule_sees_a_method_taking_text() {
    let text = r#"
mod nested {
    impl DisplayManager {
        pub(crate) fn a(&self, text: &str) {}
        pub(super) fn b(&self, text: Option<&str>) {}
        pub fn c(&self, texts: impl IntoIterator<Item = String>) {}
        pub(crate) fn d(&self, text: &impl std::fmt::Display) {}
        pub(crate) fn e<T>(&self, text: T) where T: fmt::Display {}
        pub(in crate::x) fn f<T: ToString>(&self, text: T) {}
        fn private(&self, text: &str) {}
        pub(crate) fn kind(&self, text: impl Into<Shown>, name: Name) {}
    }
}
impl<T> Choice<T> {
    pub(crate) fn g(text: String, value: T) -> Self { todo!() }
}
impl Other {
    pub(crate) fn h(&self, text: &str) {}
}
"#;

    assert_eq!(
        found_in("display_manager.rs", text),
        [
            (PRINTER.name, 4),
            (PRINTER.name, 5),
            (PRINTER.name, 6),
            (PRINTER.name, 7),
            (PRINTER.name, 8),
            (PRINTER.name, 9),
            (PRINTER.name, 15),
        ]
    );
}

// console's color state is seen set or styled through by path, through a
// grouped or renamed import, a crate alias, a glob, a Style constructor or a
// stream method.
#[test]
fn the_console_rule_sees_every_route_to_its_state() {
    let text = "use console::{Style, style};\n\
                use console as c;\n\
                use console::*;\n\
                fn a() { console::set_colors_enabled(false); }\n\
                fn b(x: &str) -> String { c::style(x).to_string() }\n\
                fn d() -> Style { Style::new().for_stderr() }\n\
                fn e() -> bool { console::colors_enabled() }\n\
                fn f() -> console::Term { console::Term::stderr() }\n";

    let found: Vec<usize> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == CONSOLE.name)
        .map(|(_, line)| line)
        .collect();
    assert_eq!(found, [1, 3, 5, 6, 7]);
    let styles: Vec<_> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == STYLE.name)
        .collect();
    assert_eq!(styles, [(STYLE.name, 6)]);
    assert!(
        found_in("terminal_text.rs", text)
            .iter()
            .all(|(rule, _)| *rule != STYLE.name)
    );
    let setters: Vec<_> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == SETTER.name)
        .collect();
    assert_eq!(setters, [(SETTER.name, 4)]);
}

// The function that reads console's answer may read it, and nothing lets it
// set the state: a setter there, called by path or through an import,
// survives the exemptions.
#[test]
fn no_exemption_lets_the_color_decision_set_consoles_state() {
    let text = "use console::set_colors_enabled_stderr as quiet;
                fn console_colors() -> StreamColors {
                    let stdout = console::colors_enabled();
                    if stdout { console::set_colors_enabled(false); }
                    quiet(false);
                    todo!()
                }
";
    let mut found = findings(&Source::parse("display_manager.rs", text));

    remove_exempt(&mut found);

    let left: Vec<(&str, usize)> = found
        .iter()
        .filter(|f| f.rule == SETTER.name || f.rule == CONSOLE.name)
        .map(|f| (f.rule, f.line))
        .collect();
    assert_eq!(left, [(SETTER.name, 1), (SETTER.name, 4), (SETTER.name, 5)]);
}

// A write to a stream outside the guarded writers is seen: a print macro, and
// std's or console's handles. A command module holds no progress bar, which
// the indicatif rule keeps to display_manager.rs, so a `println` method there
// is a printer's.
#[test]
fn the_writer_rule_sees_every_write() {
    let text = "use std::io::stdout;\n\
                fn a() { eprintln!(\"x\"); }\n\
                fn b(display: &D) { display.println(lit!(\"x\")); }\n\
                fn c() { let _ = std::io::stderr(); }\n\
                fn d() { let _ = console::Term::stdout(); }\n\
                fn e() { print!(\"x\"); }\n";

    let found: Vec<usize> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == WRITER.name)
        .map(|(_, line)| line)
        .collect();
    assert_eq!(found, [1, 2, 4, 6]);
    let handles: Vec<_> = found_in("commands/x.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == HANDLE.name)
        .collect();
    assert_eq!(handles, [(HANDLE.name, 5)]);
}

// In display_manager.rs, a progress bar's `println` is seen on any receiver
// other than the display itself: a field of another name, a bound reference,
// or a chain. A printer called on `self` or `self.display` is not.
#[test]
fn a_progress_write_is_seen_on_any_receiver() {
    let text = "impl DisplayManager {\n\
                fn a(&self) { let _ = self.multi.println(\"x\"); }\n\
                fn b(&self) { let bar = &self.mp; let _ = bar.println(\"x\"); }\n\
                fn c(&self) { let _ = self.mp.clone().println(\"x\"); }\n\
                fn d(&self) { self.println(lit!(\"x\")); }\n\
                }\n\
                impl ResultCard<'_> {\n\
                fn e(&self) { self.display.println(lit!(\"x\")); }\n\
                }\n";

    let found: Vec<usize> = found_in("display_manager.rs", text)
        .into_iter()
        .filter(|(rule, _)| *rule == WRITER.name)
        .map(|(_, line)| line)
        .collect();
    assert_eq!(found, [2, 3, 4]);
}

// indicatif is seen outside display_manager.rs, so no other module holds a bar
// whose `println` the writer rule would not see.
#[test]
fn indicatif_is_seen_outside_display_manager() {
    let text = "use indicatif::MultiProgress;\nfn a(mp: &indicatif::MultiProgress) {}\n";

    assert_eq!(
        found_in("commands/x.rs", text),
        [(INDICATIF.name, 1), (INDICATIF.name, 2)]
    );
    assert_eq!(found_in("display_manager.rs", text), []);
}

// Each library type whose sentence is built from parts, by name: the type of
// every `impl Composed for` under the library's `src`.
fn composed_types() -> Vec<String> {
    struct Impls(Vec<String>);
    impl<'ast> Visit<'ast> for Impls {
        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            let composed = match &item.trait_ {
                Some((path, _)) => path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "Composed"),
                None => false,
            };
            if composed {
                self.0.push(last_segment(&item.self_ty));
            }
            visit::visit_item_impl(self, item);
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../selfie/src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    let mut impls = Impls(Vec::new());
    for path in files {
        let file = syn::parse_file(&std::fs::read_to_string(&path).unwrap()).unwrap();
        impls.visit_file(&file);
    }
    impls.0
}

// Each type `sentence.rs` lists in `sentences!`, by name.
fn sentence_types() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sentence.rs");
    let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Macro(item) if item.mac.path.is_ident("sentences") => Some(
                item.mac
                    .parse_body_with(Punctuated::<syn::Type, Token![,]>::parse_terminated)
                    .unwrap(),
            ),
            _ => None,
        })
        .flatten()
        .map(|ty| last_segment(&ty))
        .collect()
}

// A library type whose sentence is built from parts is shown through
// `composed`, which shows each name in it as a name, so it is never also a
// `Sentence`, which `sentence()` would show as one prose sentence.
#[test]
fn no_type_built_from_parts_is_a_sentence() {
    let composed = composed_types();
    let sentences = sentence_types();
    assert!(
        composed.iter().any(|name| name == "OperationSuccess"),
        "the walk found no Composed impl it should: {composed:?}"
    );
    assert!(
        sentences.iter().any(|name| name == "PackageError"),
        "the walk found no sentences! list: {sentences:?}"
    );

    let both: Vec<&String> = sentences
        .iter()
        .filter(|name| composed.contains(name))
        .collect();
    assert_eq!(
        both,
        Vec::<&String>::new(),
        "listed in sentences! and Composed"
    );
}
