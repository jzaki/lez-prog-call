//! A guest's interface, read from its source file.
//!
//! A built guest carries no description of its messages: Borsh encodes an enum variant as its
//! index and its fields, with no names. So this parses the guest's source with `syn`: it finds
//! the handler that `actor_entrypoint!` names, takes the type of the handler's second parameter,
//! and lists that type's variants if the file defines it.
//!
//! It also reads two things the call planner uses: the file's `PdaSeed` constants, and per-message
//! hints from what each match arm checks (`is_authorized` → the receiver signs; `from` checks →
//! another program sends it). The hints only label and order: a dry run decides.

use quote::ToTokens as _;
use serde::Serialize;
use syn::{
    Attribute, Expr, ExprArray, ExprRepeat, Fields, FnArg, Item, ItemFn, Lit, Meta, Pat, Type,
    UnOp, visit::Visit,
};

use crate::hex;

/// How deep local struct params are expanded into their fields.
const MAX_EXPANSION_DEPTH: usize = 4;

/// The type of a raw handler's one param: the message bytes, as hex.
pub const RAW: &str = "raw bytes";

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Interface {
    pub handler: String,
    /// `"enum Message"`, `"struct Ping"`, a type such as `"Vec<u8>"`, or `"raw"`.
    pub message: String,
    pub methods: Vec<Method>,
    pub seeds: Vec<Seed>,
}

/// One message that can be sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Method {
    /// The variant name, the struct or type name, or `"raw"`.
    pub name: String,
    /// The Borsh variant index, for an enum.
    pub index: Option<u8>,
    /// `""`, `"(Vec<u8>)"` or `" { data: Vec<u8>, to: AccountId }"`, as declared.
    pub fields: String,
    /// The values to give, in Borsh order, with local structs expanded into their fields.
    pub params: Vec<Param>,
    /// The first line of its doc comment.
    pub doc: Option<String>,
    /// `"signed"` and/or `"internal"`.
    pub hints: Vec<&'static str>,
}

/// One value of a message. `name` is the field name, `#0`… for a tuple field, `a.b` inside an
/// expanded struct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Param {
    pub name: String,
    pub ty: String,
    /// What to type, for a form.
    pub hint: String,
    /// The variant names of a local enum whose variants are all units: the value is one of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// A `PdaSeed` constant: its name and bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Seed {
    pub name: String,
    #[serde(serialize_with = "hex_bytes")]
    pub bytes: [u8; 32],
}

/// Every `match` arm under a node.
#[derive(Default)]
struct Arms<'ast> {
    arms: Vec<&'ast syn::Arm>,
}

impl<'ast> Visit<'ast> for Arms<'ast> {
    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        self.arms.push(i);
        syn::visit::visit_arm(self, i);
    }
}

impl Interface {
    /// The message named `name` (a variant name, or the single message's name), or by index.
    pub fn method(&self, name: &str) -> Option<&Method> {
        self.methods
            .iter()
            .find(|method| method.name == name)
            .or_else(|| {
                let index: u8 = name.parse().ok()?;
                self.methods
                    .iter()
                    .find(|method| method.index == Some(index))
            })
    }

    /// What `lez-call iface` prints.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("handler: {}", self.handler),
            format!("message: {}", self.message),
            String::new(),
        ];
        for method in &self.methods {
            let index = method
                .index
                .map_or_else(|| "  ".to_owned(), |index| format!("{index:>2}"));
            let hints = if method.hints.is_empty() {
                String::new()
            } else {
                format!("   [{}]", method.hints.join(", "))
            };
            lines.push(format!("{index}  {}{}{hints}", method.name, method.fields));
            if let Some(doc) = &method.doc {
                lines.push(format!("      {doc}"));
            }
            for param in &method.params {
                lines.push(format!(
                    "      {}: {}   ({})",
                    param.name, param.ty, param.hint
                ));
            }
        }
        if !self.seeds.is_empty() {
            lines.push(String::new());
            for seed in &self.seeds {
                lines.push(format!(
                    "PdaSeed {} = 0x{}",
                    seed.name,
                    hex::encode(seed.bytes)
                ));
            }
        }
        lines.push(String::new());
        lines.push(if self.message.starts_with("enum ") {
            "Borsh: the variant's index as one byte, then its fields in order.".to_owned()
        } else if self.message == "raw" {
            "raw: the handler decodes the message bytes itself.".to_owned()
        } else {
            "Borsh: one encoded value of this type.".to_owned()
        });
        lines
    }
}

pub fn parse(source: &str) -> anyhow::Result<Interface> {
    let file = syn::parse_file(source)?;
    let items = all_items(&file.items);
    let seeds = seeds(&items);
    let (raw, handler) = items
        .iter()
        .find_map(|item| match item {
            Item::Macro(item) => entry_handler(&item.mac),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no actor_entrypoint! in this file"))?;
    let function = items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == handler => Some(function),
        _ => None,
    });
    if raw {
        let hints = function
            .map(|function| hints(&items, function, None))
            .unwrap_or_default();
        return Ok(Interface {
            handler,
            message: "raw".to_owned(),
            methods: vec![Method {
                name: "raw".to_owned(),
                index: None,
                fields: String::new(),
                params: vec![param("hex".to_owned(), RAW.to_owned(), &items)],
                doc: None,
                hints,
            }],
            seeds,
        });
    }
    let function = function
        .ok_or_else(|| anyhow::anyhow!("the handler `{handler}` isn't defined in this file"))?;
    let ty = message_type(function)
        .ok_or_else(|| anyhow::anyhow!("`{handler}` has no message parameter"))?;
    let name = match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    };
    let local = name.as_deref().and_then(|name| {
        items.iter().find_map(|item| match item {
            Item::Enum(item) if item.ident == name => Some(Item::Enum(item.clone())),
            Item::Struct(item) if item.ident == name => Some(Item::Struct(item.clone())),
            _ => None,
        })
    });
    let (message, methods) = match local {
        Some(Item::Enum(item)) => (
            format!("enum {}", item.ident),
            item.variants
                .iter()
                .enumerate()
                .map(|(index, variant)| {
                    let variant_name = variant.ident.to_string();
                    Method {
                        hints: hints(&items, function, Some(&variant_name)),
                        name: variant_name,
                        index: u8::try_from(index).ok(),
                        fields: fields(&variant.fields),
                        params: params(&variant.fields, &items, "", 0),
                        doc: doc(&variant.attrs),
                    }
                })
                .collect(),
        ),
        Some(Item::Struct(item)) => (
            format!("struct {}", item.ident),
            vec![Method {
                name: item.ident.to_string(),
                index: None,
                fields: fields(&item.fields),
                params: params(&item.fields, &items, "", 0),
                doc: doc(&item.attrs),
                hints: hints(&items, function, None),
            }],
        ),
        _ => {
            let ty = tidy(&ty.to_token_stream().to_string());
            (
                ty.clone(),
                vec![Method {
                    name: ty.clone(),
                    index: None,
                    fields: String::new(),
                    params: expand("#0".to_owned(), ty, &items, 0),
                    doc: None,
                    hints: hints(&items, function, None),
                }],
            )
        }
    };
    Ok(Interface {
        handler,
        message,
        methods,
        seeds,
    })
}

/// Top-level items, and those of inline modules.
fn all_items(items: &[Item]) -> Vec<&Item> {
    let mut found = Vec::new();
    for item in items {
        found.push(item);
        if let Item::Mod(syn::ItemMod {
            content: Some((_, inner)),
            ..
        }) = item
        {
            found.extend(all_items(inner));
        }
    }
    found
}

/// `(raw, handler)` from `actor_entrypoint!(raw path::to::handler)`.
fn entry_handler(mac: &syn::Macro) -> Option<(bool, String)> {
    if mac.path.segments.last()?.ident != "actor_entrypoint" {
        return None;
    }
    let args = tidy(&mac.tokens.to_string());
    let args = args.trim().trim_end_matches(',').trim();
    let (raw, path) = args
        .strip_prefix("raw ")
        .map_or((false, args), |path| (true, path.trim()));
    let handler = path.rsplit("::").next()?.to_owned();
    Some((raw, handler))
}

fn message_type(function: &ItemFn) -> Option<&Type> {
    match function.sig.inputs.iter().nth(1)? {
        FnArg::Typed(arg) => Some(&arg.ty),
        FnArg::Receiver(_) => None,
    }
}

/// The params of a variant's or struct's fields, under `prefix`.
fn params(fields: &Fields, items: &[&Item], prefix: &str, depth: usize) -> Vec<Param> {
    fields
        .iter()
        .enumerate()
        .flat_map(|(index, field)| {
            let name = field
                .ident
                .as_ref()
                .map_or_else(|| format!("#{index}"), ToString::to_string);
            let ty = tidy(&field.ty.to_token_stream().to_string());
            expand(format!("{prefix}{name}"), ty, items, depth)
        })
        .collect()
}

/// One param, or a local struct's fields in its place.
fn expand(name: String, ty: String, items: &[&Item], depth: usize) -> Vec<Param> {
    let local = items.iter().find_map(|item| match item {
        Item::Struct(item) if item.ident == short(&ty) && item.generics.params.is_empty() => {
            Some(item)
        }
        _ => None,
    });
    match local {
        Some(item) if depth < MAX_EXPANSION_DEPTH && !item.fields.is_empty() => params(
            &item.fields,
            items,
            &format!("{name}."),
            depth.saturating_add(1),
        ),
        _ => vec![param(name, ty, items)],
    }
}

fn param(name: String, ty: String, items: &[&Item]) -> Param {
    let choices = unit_variants(&ty, items);
    let hint = if choices.is_empty() {
        hint(&ty)
    } else {
        format!("one of {}", choices.join(" | "))
    };
    Param {
        name,
        ty,
        hint,
        choices,
    }
}

/// The variants of a local enum named `ty` whose variants are all units.
fn unit_variants(ty: &str, items: &[&Item]) -> Vec<String> {
    items
        .iter()
        .find_map(|item| match item {
            Item::Enum(item)
                if item.ident == short(ty)
                    && item
                        .variants
                        .iter()
                        .all(|variant| matches!(variant.fields, Fields::Unit)) =>
            {
                Some(
                    item.variants
                        .iter()
                        .map(|variant| variant.ident.to_string())
                        .collect(),
                )
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// `""`, `"(Vec<u8>)"` or `" { data: Vec<u8>, to: AccountId }"`.
fn fields(fields: &Fields) -> String {
    let declared: Vec<(Option<String>, String)> = fields
        .iter()
        .map(|field| {
            (
                field.ident.as_ref().map(ToString::to_string),
                tidy(&field.ty.to_token_stream().to_string()),
            )
        })
        .collect();
    match fields {
        Fields::Unit => String::new(),
        Fields::Unnamed(_) => {
            let types: Vec<&str> = declared.iter().map(|(_, ty)| ty.as_str()).collect();
            format!("({})", types.join(", "))
        }
        Fields::Named(_) => {
            let named: Vec<String> = declared
                .iter()
                .map(|(name, ty)| format!("{}: {ty}", name.as_deref().unwrap_or_default()))
                .collect();
            format!(" {{ {} }}", named.join(", "))
        }
    }
}

/// What a message's handling checks: the match arm for `variant` (or the whole handler), with
/// the bodies of the local functions it calls.
fn hints(items: &[&Item], handler: &ItemFn, variant: Option<&str>) -> Vec<&'static str> {
    let body = match variant {
        None => handler.block.to_token_stream().to_string(),
        Some(variant) => {
            let mut arms = Arms::default();
            arms.visit_block(&handler.block);
            let Some(arm) = arms
                .arms
                .into_iter()
                .find(|arm| arm_names(&arm.pat, variant))
            else {
                return Vec::new();
            };
            arm.body.to_token_stream().to_string()
        }
    };
    let mut text = body.clone();
    for item in items {
        if let Item::Fn(function) = item {
            let name = &function.sig.ident;
            if *name != handler.sig.ident && body.contains(&format!("{name} (")) {
                text.push(' ');
                text.push_str(&function.block.to_token_stream().to_string());
            }
        }
    }
    let mut hints = Vec::new();
    if text.contains("is_authorized") {
        hints.push("signed");
    }
    if [
        "from_own_program",
        "from . expect",
        "from . unwrap",
        "from . is_some ()",
    ]
    .iter()
    .any(|check| text.contains(check))
    {
        hints.push("internal");
    }
    hints
}

/// Whether a match pattern names `variant` (`Message::Open(x)`, `Message::Mint { .. }`, …).
fn arm_names(pat: &Pat, variant: &str) -> bool {
    let path = match pat {
        Pat::TupleStruct(pat) => &pat.path,
        Pat::Struct(pat) => &pat.path,
        Pat::Path(pat) => &pat.path,
        Pat::Or(pat) => return pat.cases.iter().any(|case| arm_names(case, variant)),
        _ => return false,
    };
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == variant)
}

/// The file's `PdaSeed` constants and statics with a literal value.
fn seeds(items: &[&Item]) -> Vec<Seed> {
    items
        .iter()
        .filter_map(|item| {
            let (ident, ty, expr) = match item {
                Item::Const(item) => (&item.ident, &*item.ty, &*item.expr),
                Item::Static(item) => (&item.ident, &*item.ty, &*item.expr),
                _ => return None,
            };
            let is_seed = matches!(ty, Type::Path(path)
                if path.path.segments.last().is_some_and(|segment| segment.ident == "PdaSeed"));
            if !is_seed {
                return None;
            }
            let Expr::Call(call) = expr else {
                return None;
            };
            let bytes = seed_bytes(call.args.first()?)?;
            Some(Seed {
                name: ident.to_string(),
                bytes,
            })
        })
        .collect()
}

/// `[37; 32]`, `[1, 2, …]` or `*b"…"`, as 32 bytes.
fn seed_bytes(expr: &Expr) -> Option<[u8; 32]> {
    let bytes: Vec<u8> = match expr {
        Expr::Repeat(ExprRepeat { expr, len, .. }) => {
            let value = int_lit(expr)?;
            let len = usize::from(int_lit(len)?);
            vec![value; len]
        }
        Expr::Array(ExprArray { elems, .. }) => elems.iter().map(int_lit).collect::<Option<_>>()?,
        Expr::Unary(unary) if matches!(unary.op, UnOp::Deref(_)) => match &*unary.expr {
            Expr::Lit(lit) => match &lit.lit {
                Lit::ByteStr(text) => text.value(),
                _ => return None,
            },
            _ => return None,
        },
        _ => return None,
    };
    bytes.try_into().ok()
}

fn int_lit(expr: &Expr) -> Option<u8> {
    match expr {
        Expr::Lit(lit) => match &lit.lit {
            Lit::Int(value) => value.base10_parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// A form hint for a param's type.
pub fn hint(ty: &str) -> String {
    let ty = short(ty);
    if let Some(inner) = generic(ty, "Option") {
        return format!("empty or none, or {}", hint(inner));
    }
    if ty != "Vec<u8>"
        && let Some(inner) = generic(ty, "Vec")
    {
        return format!("comma-separated: {}", hint(inner));
    }
    match ty {
        "Vec<u8>" => "text, or 0x… hex",
        "String" => "text",
        "bool" => "true / false",
        "u8" | "u16" | "u32" | "u64" | "u128" | "i8" | "i16" | "i32" | "i64" | "i128" => "decimal",
        "AccountId" => "account: base58, Public/…, or a wallet label",
        "Actor" => "account@program",
        "PdaSeed" => "0x… hex (32 bytes) or base58",
        _ if byte_array_len(ty).is_some() => "0x… hex",
        _ => "hex of its Borsh encoding",
    }
    .to_owned()
}

/// The last path segment of a type: `lee_core::account::AccountId` → `AccountId`.
pub fn short(ty: &str) -> &str {
    let head = ty.split('<').next().unwrap_or(ty);
    head.rfind("::")
        .map_or(ty, |at| ty.get(at.saturating_add(2)..).unwrap_or(ty))
}

/// `T` from `Name<T>`.
pub fn generic<'ty>(ty: &'ty str, name: &str) -> Option<&'ty str> {
    short(ty)
        .strip_prefix(name)?
        .strip_prefix('<')?
        .strip_suffix('>')
        .map(str::trim)
}

/// `N` for `[u8; N]`.
pub fn byte_array_len(ty: &str) -> Option<usize> {
    ty.strip_prefix("[u8;")?
        .strip_suffix(']')?
        .trim()
        .parse()
        .ok()
}

fn doc(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| match &attr.meta {
        Meta::NameValue(meta) if meta.path.is_ident("doc") => match &meta.value {
            Expr::Lit(expr) => match &expr.lit {
                Lit::Str(text) => Some(text.value().trim().to_owned()),
                _ => None,
            },
            _ => None,
        },
        Meta::Path(_) | Meta::List(_) | Meta::NameValue(_) => None,
    })
}

/// Undoes the spacing of a token stream's `to_string`: `Vec < u8 >` → `Vec<u8>`.
fn tidy(tokens: &str) -> String {
    let mut text = tokens.to_owned();
    for (from, to) in [
        (" :: ", "::"),
        (":: ", "::"),
        (" ::", "::"),
        (" < ", "<"),
        ("< ", "<"),
        (" <", "<"),
        (" >", ">"),
        (" ,", ","),
        (" ;", ";"),
        ("& ", "&"),
        ("[ ", "["),
        (" ]", "]"),
        ("( ", "("),
        (" )", ")"),
    ] {
        text = text.replace(from, to);
    }
    text
}

fn hex_bytes<S: serde::Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("0x{}", hex::encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_line(method: &Method) -> String {
        format!("{}{}", method.name, method.fields)
    }

    #[test]
    fn a_plain_greeting_is_one_vec() {
        let source = "
            use lee_core::program::{ReceiveInput, Response};
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, greeting: Vec<u8>) -> Response { todo!() }
        ";
        let interface = parse(source).unwrap();
        assert_eq!(interface.handler, "handle_message");
        assert_eq!(interface.message, "Vec<u8>");
        assert_eq!(interface.methods.len(), 1);
        assert_eq!(interface.methods[0].index, None);
        assert_eq!(interface.methods[0].params[0].name, "#0");
        assert_eq!(interface.methods[0].params[0].ty, "Vec<u8>");
    }

    #[test]
    fn an_enum_lists_its_variants_in_borsh_order() {
        let source = "
            #[derive(borsh::BorshSerialize, borsh::BorshDeserialize)]
            enum Message {
                /// Appends the bytes.
                Write(Vec<u8>),
                MoveData { data: Vec<u8>, to: AccountId },
                Stamp,
                Fixed([u8; 32], lee_core::account::AccountId),
            }
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, message: Message) -> Response { todo!() }
        ";
        let interface = parse(source).unwrap();
        assert_eq!(interface.message, "enum Message");
        let shown: Vec<String> = interface.methods.iter().map(method_line).collect();
        assert_eq!(
            shown,
            [
                "Write(Vec<u8>)",
                "MoveData { data: Vec<u8>, to: AccountId }",
                "Stamp",
                "Fixed([u8; 32], lee_core::account::AccountId)",
            ]
        );
        assert_eq!(
            interface.methods[0].doc.as_deref(),
            Some("Appends the bytes.")
        );
        assert_eq!(interface.methods[1].doc, None);
        assert_eq!(interface.methods[3].index, Some(3));
        let names: Vec<&str> = interface.methods[1]
            .params
            .iter()
            .map(|param| param.name.as_str())
            .collect();
        assert_eq!(names, ["data", "to"]);
        assert_eq!(interface.method("Stamp").unwrap().index, Some(2));
        assert_eq!(interface.method("2").unwrap().name, "Stamp");
    }

    #[test]
    fn the_raw_form_and_a_path_handler() {
        let raw = parse("lee_core::actor_entrypoint!(raw handle);").unwrap();
        assert_eq!(raw.message, "raw");
        assert_eq!(raw.methods[0].params[0].ty, RAW);

        let source = "
            lee_core::actor_entrypoint!(self::inner::run);
            mod inner {
                pub struct Ping { pub to: AccountId }
                pub fn run(input: &ReceiveInput, ping: Ping) -> Response { todo!() }
            }
        ";
        let interface = parse(source).unwrap();
        assert_eq!(interface.message, "struct Ping");
        assert_eq!(method_line(&interface.methods[0]), "Ping { to: AccountId }");
        assert_eq!(interface.methods[0].params[0].name, "to");
    }

    #[test]
    fn a_file_without_an_entry_point_is_an_error() {
        assert!(parse("fn main() {}").is_err());
    }

    #[test]
    fn local_structs_and_unit_enums_in_params_are_expanded() {
        let source = "
            struct Range { start: u64, end: u64 }
            enum Side { Buy, Sell }
            enum Message { Order { side: Side, range: Range, tag: Option<u8> } }
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, message: Message) -> Response { todo!() }
        ";
        let interface = parse(source).unwrap();
        let params: Vec<(&str, &str)> = interface.methods[0]
            .params
            .iter()
            .map(|param| (param.name.as_str(), param.ty.as_str()))
            .collect();
        assert_eq!(
            params,
            [
                ("side", "Side"),
                ("range.start", "u64"),
                ("range.end", "u64"),
                ("tag", "Option<u8>")
            ]
        );
        assert_eq!(interface.methods[0].params[0].choices, ["Buy", "Sell"]);
    }

    #[test]
    fn hints_come_from_each_arms_checks_and_the_functions_it_calls() {
        let source = r#"
            enum Message { Open(Vec<u8>), Init(Vec<u8>), Credit(u128), Hello }
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, message: Message) -> Response {
                match message {
                    Message::Open(config) => {
                        assert_admin(input);
                        Response::keep_state()
                    }
                    Message::Init(config) => {
                        let seat = input.from.expect("from a seat");
                        Response::keep_state()
                    }
                    Message::Credit(amount) => {
                        assert!(input.from_own_program());
                        Response::keep_state()
                    }
                    Message::Hello => Response::keep_state(),
                }
            }
            fn assert_admin(input: &ReceiveInput) {
                assert!(input.is_authorized && input.from.is_none());
            }
        "#;
        let interface = parse(source).unwrap();
        let hints: Vec<Vec<&str>> = interface
            .methods
            .iter()
            .map(|method| method.hints.clone())
            .collect();
        assert_eq!(
            hints,
            [vec!["signed"], vec!["internal"], vec!["internal"], vec![]]
        );
    }

    #[test]
    fn pda_seed_constants_are_read() {
        let source = r#"
            const PDA_SEED: PdaSeed = PdaSeed::new([37; 32]);
            const NAMED: lee_core::program::PdaSeed = PdaSeed::new(*b"0123456789abcdef0123456789abcdef");
            const OTHER: u8 = 3;
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, callee: AccountId) -> Response { todo!() }
        "#;
        let seeds = parse(source).unwrap().seeds;
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].name, "PDA_SEED");
        assert_eq!(seeds[0].bytes, [37; 32]);
        assert_eq!(&seeds[1].bytes[..4], b"0123");
    }

    #[test]
    fn types_are_shortened_and_unwrapped() {
        assert_eq!(short("lee_core::account::AccountId"), "AccountId");
        assert_eq!(
            short("Vec<lee_core::account::AccountId>"),
            "Vec<lee_core::account::AccountId>"
        );
        assert_eq!(generic("Option<u8>", "Option"), Some("u8"));
        assert_eq!(generic("std::vec::Vec<u64>", "Vec"), Some("u64"));
        assert_eq!(byte_array_len("[u8; 32]"), Some(32));
    }

    /// Every LEZ example guest parses, with the hints and seeds a planner uses. They're read from
    /// `$LEZ_DIR` (default: a `logos-execution-zone` checkout next to this one); without one, the
    /// test has nothing to read.
    #[test]
    fn the_example_guests_as_they_are() {
        let lez = std::env::var_os("LEZ_DIR").map_or_else(
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../logos-execution-zone"),
            std::path::PathBuf::from,
        );
        let dir = lez.join("examples/program_deployment/methods/guest/src/bin");
        if !dir.is_dir() {
            eprintln!("no LEZ checkout at {}: skipped", lez.display());
            return;
        }
        let read = |name: &str| {
            parse(&std::fs::read_to_string(dir.join(format!("{name}.rs"))).unwrap()).unwrap()
        };
        let hints =
            |interface: &Interface, name: &str| interface.method(name).unwrap().hints.clone();

        let auth = read("hello_world_with_authorization");
        assert_eq!(auth.methods[0].hints, ["signed"]);
        assert!(read("hello_world").methods[0].hints.is_empty());

        let moves = read("hello_world_with_move_function");
        assert!(hints(&moves, "MoveData").is_empty());
        assert_eq!(hints(&moves, "Append"), ["internal"]);

        let pda = read("tail_call_with_pda");
        assert_eq!(pda.seeds[0].name, "PDA_SEED");
        assert_eq!(pda.seeds[0].bytes, [37; 32]);
        assert_eq!(read("simple_tail_call").methods[0].params[0].ty, "Actor");
    }
}
