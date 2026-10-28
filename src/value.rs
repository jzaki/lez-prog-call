//! Typed text → Borsh bytes, for a message's params.

use anyhow::{Context as _, bail, ensure};

use crate::{
    hex,
    iface::{self, Method, Param, RAW},
};

/// An account id: 32 bytes, written as base58.
pub type AccountId = [u8; 32];

/// Resolves an account that isn't base58: a wallet label, say.
pub type Resolve<'resolve> = &'resolve dyn Fn(&str) -> Option<AccountId>;

/// The value for each of `method`'s params, in order, from `name=value` and positional `args`.
/// A missing `Option` param is `none`.
pub fn assign(method: &Method, args: &[String]) -> anyhow::Result<Vec<String>> {
    let mut values: Vec<Option<String>> = vec![None; method.params.len()];
    let mut next = 0_usize;
    for arg in args {
        let named = arg.split_once('=').and_then(|(name, value)| {
            let at = method.params.iter().position(|param| param.name == name)?;
            Some((at, value))
        });
        if let Some((at, value)) = named {
            values[at] = Some(value.to_owned());
        } else {
            while next < values.len() && values[next].is_some() {
                next = next.saturating_add(1);
            }
            ensure!(
                next < values.len(),
                "`{}` takes {} value(s); {arg:?} is one too many",
                method.name,
                values.len()
            );
            values[next] = Some(arg.clone());
        }
    }
    method
        .params
        .iter()
        .zip(values)
        .map(|(param, value)| match value {
            Some(value) => Ok(value),
            None if iface::generic(&param.ty, "Option").is_some() => Ok(String::new()),
            None => bail!(
                "`{}` needs {}: {} ({})",
                method.name,
                param.name,
                param.ty,
                param.hint
            ),
        })
        .collect()
}

/// The Borsh-encoded message: the variant index (for an enum), then each param's value.
pub fn encode_message(
    method: &Method,
    values: &[String],
    resolve: Resolve<'_>,
) -> anyhow::Result<Vec<u8>> {
    let mut bytes: Vec<u8> = method.index.into_iter().collect();
    for (param, value) in method.params.iter().zip(values) {
        let encoded = encode_param(param, value, resolve)
            .with_context(|| format!("{}: {}", param.name, param.ty))?;
        bytes.extend(encoded);
    }
    Ok(bytes)
}

fn encode_param(param: &Param, input: &str, resolve: Resolve<'_>) -> anyhow::Result<Vec<u8>> {
    if param.choices.is_empty() {
        return encode(&param.ty, input, resolve);
    }
    let input = input.trim();
    let index = param
        .choices
        .iter()
        .position(|choice| choice == input)
        .or_else(|| input.parse().ok())
        .filter(|&index| index < param.choices.len())
        .ok_or_else(|| anyhow::anyhow!("expected one of {}", param.choices.join(" | ")))?;
    Ok(vec![u8::try_from(index)?])
}

/// The Borsh encoding of one value of type `ty`. Types it doesn't know take the hex of their
/// encoding.
pub fn encode(ty: &str, input: &str, resolve: Resolve<'_>) -> anyhow::Result<Vec<u8>> {
    let short = iface::short(ty);
    // Text is taken as typed; anything else may have spaces around it.
    if let "Vec<u8>" | "String" = short
        && !input.trim_start().starts_with("0x")
    {
        return with_length(input.as_bytes());
    }
    let input = input.trim();
    if ty == RAW {
        return unhex(input);
    }
    if let Some(inner) = iface::generic(short, "Option") {
        return if input.is_empty() || input == "none" {
            Ok(vec![0])
        } else {
            Ok([vec![1], encode(inner, input, resolve)?].concat())
        };
    }
    if short != "Vec<u8>"
        && let Some(inner) = iface::generic(short, "Vec")
    {
        let items: Vec<&str> = if input.is_empty() {
            Vec::new()
        } else {
            input.split(',').collect()
        };
        let mut bytes = u32::try_from(items.len())?.to_le_bytes().to_vec();
        for item in items {
            bytes.extend(encode(inner, item, resolve)?);
        }
        return Ok(bytes);
    }
    match short {
        "Vec<u8>" => match input.strip_prefix("0x") {
            Some(hex) => with_length(&unhex(hex)?),
            None => with_length(input.as_bytes()),
        },
        "String" => with_length(input.as_bytes()),
        "bool" => match input {
            "true" => Ok(vec![1]),
            "false" => Ok(vec![0]),
            _ => bail!("expected true or false, not {input:?}"),
        },
        "u8" => Ok(input.parse::<u8>()?.to_le_bytes().to_vec()),
        "u16" => Ok(input.parse::<u16>()?.to_le_bytes().to_vec()),
        "u32" => Ok(input.parse::<u32>()?.to_le_bytes().to_vec()),
        "u64" => Ok(input.parse::<u64>()?.to_le_bytes().to_vec()),
        "u128" => Ok(input.parse::<u128>()?.to_le_bytes().to_vec()),
        "i8" => Ok(input.parse::<i8>()?.to_le_bytes().to_vec()),
        "i16" => Ok(input.parse::<i16>()?.to_le_bytes().to_vec()),
        "i32" => Ok(input.parse::<i32>()?.to_le_bytes().to_vec()),
        "i64" => Ok(input.parse::<i64>()?.to_le_bytes().to_vec()),
        "i128" => Ok(input.parse::<i128>()?.to_le_bytes().to_vec()),
        "AccountId" | "PdaSeed" => Ok(account(input, resolve)?.to_vec()),
        "Actor" => {
            let (account_id, program) = input
                .split_once('@')
                .or_else(|| input.split_once(','))
                .ok_or_else(|| anyhow::anyhow!("expected `account@program`"))?;
            Ok([
                account(account_id, resolve)?.to_vec(),
                account(program, resolve)?.to_vec(),
            ]
            .concat())
        }
        _ => {
            let bytes = unhex(input)?;
            if let Some(len) = iface::byte_array_len(short) {
                ensure!(
                    bytes.len() == len,
                    "expected {len} bytes, got {}",
                    bytes.len()
                );
            }
            Ok(bytes)
        }
    }
}

/// An account id: base58 (with or without `Public/`), `0x` hex, or what `resolve` finds.
pub fn account(text: &str, resolve: Resolve<'_>) -> anyhow::Result<AccountId> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x") {
        return unhex(hex)?
            .try_into()
            .map_err(|bytes: Vec<u8>| anyhow::anyhow!("expected 32 bytes, got {}", bytes.len()));
    }
    let bare = text.strip_prefix("Public/").unwrap_or(text);
    if let Some(account_id) = from_base58(bare) {
        return Ok(account_id);
    }
    resolve(text).ok_or_else(|| anyhow::anyhow!("{text:?} isn't base58, 0x hex, or a wallet label"))
}

pub fn from_base58(text: &str) -> Option<AccountId> {
    bs58::decode(text).into_vec().ok()?.try_into().ok()
}

pub fn to_base58(account_id: &AccountId) -> String {
    bs58::encode(account_id).into_string()
}

fn with_length(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = u32::try_from(data.len())?.to_le_bytes().to_vec();
    bytes.extend(data);
    Ok(bytes)
}

pub fn unhex(text: &str) -> anyhow::Result<Vec<u8>> {
    let text: String = text
        .trim()
        .trim_start_matches("0x")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    hex::decode(&text).with_context(|| format!("not hex: {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_ACCOUNT: &str = "
        enum Message {
            Hello(Vec<u8>),
            WriteAccountId,
            MoveData { data: Vec<u8>, to: AccountId },
        }
        lee_core::actor_entrypoint!(handle_message);
        fn handle_message(input: &ReceiveInput, message: Message) -> Response { todo!() }
    ";

    fn none(_: &str) -> Option<AccountId> {
        None
    }

    fn send(method: &Method, args: &[&str]) -> anyhow::Result<String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let values = assign(method, &args)?;
        encode_message(method, &values, &none).map(hex::encode)
    }

    #[test]
    fn methods_encode_their_variant_and_params() {
        let methods = iface::parse(HELLO_ACCOUNT).unwrap().methods;
        assert_eq!(send(&methods[0], &["Hi"]).unwrap(), "00020000004869");
        assert_eq!(send(&methods[0], &["0xff00"]).unwrap(), "0002000000ff00");
        assert_eq!(send(&methods[1], &[]).unwrap(), "01");

        let id = to_base58(&[7; 32]);
        let expected = format!("0202000000{}{}", "4869", "07".repeat(32));
        assert_eq!(
            send(&methods[2], &["Hi", &format!("Public/{id}")]).unwrap(),
            expected
        );
        assert_eq!(
            send(&methods[2], &[&format!("to={id}"), "data=Hi"]).unwrap(),
            expected
        );
        assert!(send(&methods[2], &["Hi"]).is_err());
        assert!(send(&methods[1], &["extra"]).is_err());
    }

    #[test]
    fn values_are_checked() {
        let encoded = |ty: &str, input: &str| encode(ty, input, &none).map(hex::encode);
        assert_eq!(encoded("u64", "258").unwrap(), "0201000000000000");
        assert_eq!(encoded("bool", "true").unwrap(), "01");
        assert_eq!(encoded("[u8; 2]", "0xabcd").unwrap(), "abcd");
        assert_eq!(encoded("Option<u8>", "").unwrap(), "00");
        assert_eq!(encoded("Option<u8>", "5").unwrap(), "0105");
        assert_eq!(encoded("Vec<u16>", "1,2").unwrap(), "0200000001000200");
        let id = to_base58(&[1; 32]);
        assert_eq!(
            encode("Actor", &format!("{id}@{id}"), &none).unwrap(),
            vec![1; 64]
        );
        assert_eq!(
            encode("Actor", &format!("{id},{id}"), &none).unwrap(),
            vec![1; 64]
        );

        assert!(encoded("u8", "256").is_err());
        assert!(encoded("u64", "ten").is_err());
        assert!(encoded("[u8; 2]", "abc").is_err());
        assert!(encoded("[u8; 2]", "abcdef").is_err());
        assert!(encoded("AccountId", "not-base58!").is_err());
    }

    #[test]
    fn labels_resolve_and_unit_enums_take_a_name() {
        let label = |text: &str| (text == "alice").then_some([9; 32]);
        assert_eq!(account("alice", &label).unwrap(), [9; 32]);
        assert!(account("bob", &label).is_err());

        let source = "
            enum Side { Buy, Sell }
            enum Message { Order(Side) }
            lee_core::actor_entrypoint!(handle_message);
            fn handle_message(input: &ReceiveInput, message: Message) -> Response { todo!() }
        ";
        let order = &iface::parse(source).unwrap().methods[0];
        assert_eq!(send(order, &["Sell"]).unwrap(), "0001");
        assert_eq!(send(order, &["0"]).unwrap(), "0000");
        assert!(send(order, &["Hold"]).is_err());
    }

    #[test]
    fn a_raw_handler_takes_hex() {
        let methods = iface::parse("lee_core::actor_entrypoint!(raw handle);")
            .unwrap()
            .methods;
        assert_eq!(send(&methods[0], &["0a 0b"]).unwrap(), "0a0b");
    }
}
