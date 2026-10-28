//! Calls any LEZ program, from its guest's source.
//!
//! A built guest carries no description of its messages, so `lez-call` reads the guest's source:
//! the messages its handler takes and their params in Borsh order (`iface`), and encodes a message
//! from typed values (`encode`). Sending is the wallet's: `call` runs `wallet program call` with
//! the encoded message and the actors given. Nothing here reads the chain.
//!
//! With `--json`, results are one `LEZCALL {json}` line.

use std::{
    cell::OnceCell,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use anyhow::{Context as _, bail};
use serde_json::json;

use crate::{
    iface::{Interface, Method},
    value::{AccountId, to_base58},
};

mod actor;
mod hex;
mod iface;
mod value;

const USAGE: &str = "\
lez-call: a LEZ program's interface from its guest source, its messages encoded, and calls
through the wallet.

Usage:
  lez-call iface  <guest.rs>                                  the messages and their params
  lez-call encode <guest.rs> [<Method>] [values]…             a message, Borsh-encoded, as hex
  lez-call call   <guest.rs> --program <id> --to <actor> [--actor <actor>]… [--payer <account>]
                  [--print] [<Method>] [values]…              encode, then `wallet program call`

A program with one message takes just its values. Values are `name=value`, or in order.

Options:
  --json                one `LEZCALL {json}` line instead of text
  --label <name>=<id>   a label for an account value (otherwise `wallet account list` is asked)
  --print               print the wallet command instead of running it

An account value is base58, `Public/<base58>`, 0x hex, or a wallet label. An actor is as
`wallet program call` takes it (`<account>[@program][:sign]`, `pda:[<owner>/]<seed>[@program]`,
`pk:<hex>[@program]`); a PDA's seed may also be the name of a `PdaSeed` constant in the source.

The wallet is `$LEZ_WALLET`, or `wallet` on PATH, run with this environment
(`LEE_WALLET_HOME_DIR`, `LEE_WALLET_PASSWORD`).";

#[derive(Default)]
struct Args {
    json: bool,
    print: bool,
    labels: Vec<(String, AccountId)>,
    program: Option<String>,
    to: Option<String>,
    actors: Vec<String>,
    payer: Option<String>,
    words: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args = parse_args(std::env::args().skip(1))?;
    let Some((command, words)) = args.words.split_first() else {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    };
    let Some((source, words)) = words.split_first() else {
        bail!("`{command}` needs a guest source file\n\n{USAGE}");
    };
    let source = Path::new(source);
    let interface = read_interface(source)?;
    match command.as_str() {
        "iface" => {
            if args.json {
                let mut value = serde_json::to_value(&interface)?;
                value["source"] = json!(source.display().to_string());
                println!("LEZCALL {value}");
            } else {
                println!("{}", source.display());
                for line in interface.lines() {
                    println!("{line}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "encode" => {
            let encoded = encode(&interface, words, &args)?;
            if args.json {
                println!("LEZCALL {}", encoded.to_json());
            } else {
                println!("0x{}", hex::encode(&encoded.message));
            }
            Ok(ExitCode::SUCCESS)
        }
        "call" => call(&interface, words, &args),
        _ => bail!("no command `{command}`\n\n{USAGE}"),
    }
}

fn parse_args(raw: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut raw = raw.peekable();
    while let Some(word) = raw.next() {
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) if word.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (word.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| raw.next())
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--json" => args.json = true,
            "--print" => args.print = true,
            "--program" => args.program = Some(value()?),
            "--to" => args.to = Some(value()?),
            "--actor" => args.actors.push(value()?),
            "--payer" => args.payer = Some(value()?),
            "--label" => {
                let text = value()?;
                let (name, id) = text
                    .split_once('=')
                    .with_context(|| format!("--label {text}: expected <name>=<id>"))?;
                let id = value::from_base58(id.trim_start_matches("Public/"))
                    .with_context(|| format!("--label {text}: {id:?} isn't base58"))?;
                args.labels.push((name.to_owned(), id));
            }
            _ if word.starts_with("--") => bail!("no option {word}\n\n{USAGE}"),
            _ => args.words.push(word),
        }
    }
    Ok(args)
}

fn read_interface(source: &Path) -> anyhow::Result<Interface> {
    let text = std::fs::read_to_string(source)
        .with_context(|| format!("can't read {}", source.display()))?;
    iface::parse(&text).with_context(|| source.display().to_string())
}

/// A message, encoded from a method and its values.
struct Encoded<'interface> {
    method: &'interface Method,
    message: Vec<u8>,
    /// The account ids its values name, for a planner.
    mentioned: Vec<AccountId>,
}

impl Encoded<'_> {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "method": self.method.name,
            "index": self.method.index,
            "hints": self.method.hints,
            "message_hex": format!("0x{}", hex::encode(&self.message)),
            "mentioned": self.mentioned.iter().map(to_base58).collect::<Vec<_>>(),
        })
    }
}

fn encode<'interface>(
    interface: &'interface Interface,
    words: &[String],
    args: &Args,
) -> anyhow::Result<Encoded<'interface>> {
    // A program with one message needs no name for it: what's given are its values.
    let named = words.first().and_then(|name| interface.method(name));
    let (method, values) = match (named, interface.methods.as_slice()) {
        (Some(method), _) => (method, words.get(1..).unwrap_or_default()),
        (None, [only]) => (only, words),
        (None, methods) => {
            let names: Vec<&str> = methods.iter().map(|method| method.name.as_str()).collect();
            bail!(
                "no message {:?}: it takes {}",
                words.first().map(String::as_str).unwrap_or_default(),
                names.join(", ")
            );
        }
    };
    let wallet_labels = OnceCell::new();
    let resolve = |text: &str| {
        args.labels
            .iter()
            .chain(wallet_labels.get_or_init(wallet_labels_or_none))
            .find(|(label, _)| label == text)
            .map(|(_, id)| *id)
    };
    let values = value::assign(method, values)?;
    let message = value::encode_message(method, &values, &resolve)?;
    let mentioned = method
        .params
        .iter()
        .zip(&values)
        .flat_map(|(param, value)| match iface::short(&param.ty) {
            "AccountId" | "PdaSeed" => vec![value.as_str()],
            "Actor" => value.split(['@', ',']).collect(),
            _ => Vec::new(),
        })
        .filter_map(|text| value::account(text, &resolve).ok())
        .collect();
    Ok(Encoded {
        method,
        message,
        mentioned,
    })
}

fn call(interface: &Interface, words: &[String], args: &Args) -> anyhow::Result<ExitCode> {
    let program = args.program.as_deref().context("`call` needs --program")?;
    let to = args.to.as_deref().context("`call` needs --to")?;
    let encoded = encode(interface, words, args)?;
    let mut wallet_args = vec![
        "program".to_owned(),
        "call".to_owned(),
        "--program".to_owned(),
        program.to_owned(),
        "--message-hex".to_owned(),
        format!("0x{}", hex::encode(&encoded.message)),
        "--to".to_owned(),
        actor::with_seed_hex(to, &interface.seeds),
    ];
    for actor in &args.actors {
        wallet_args.push("--actor".to_owned());
        wallet_args.push(actor::with_seed_hex(actor, &interface.seeds));
    }
    if let Some(payer) = &args.payer {
        wallet_args.push("--payer".to_owned());
        wallet_args.push(payer.clone());
    }
    let line = std::iter::once("wallet")
        .chain(wallet_args.iter().map(String::as_str))
        .map(shell_word)
        .collect::<Vec<_>>()
        .join(" ");
    if args.print {
        if args.json {
            let mut value = encoded.to_json();
            value["command"] = json!(line);
            value["wallet_args"] = json!(wallet_args);
            println!("LEZCALL {value}");
        } else {
            println!("{line}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let status = Command::new(wallet())
        .args(&wallet_args)
        .status()
        .with_context(|| format!("can't run {}", wallet().to_string_lossy()))?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

fn wallet() -> OsString {
    std::env::var_os("LEZ_WALLET").unwrap_or_else(|| PathBuf::from("wallet").into_os_string())
}

/// The wallet's labelled public accounts, from `wallet account list`; none when it can't be run.
fn wallet_labels_or_none() -> Vec<(String, AccountId)> {
    let Ok(output) = Command::new(wallet()).args(["account", "list"]).output() else {
        return Vec::new();
    };
    parse_account_list(&String::from_utf8_lossy(&output.stdout))
}

/// The labelled public accounts in `wallet account list`'s output, whose lines look like
/// `/0 Public/5QBN… [label]`.
fn parse_account_list(output: &str) -> Vec<(String, AccountId)> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("Public/")?;
            let (id, rest) = rest.split_once(' ')?;
            let label = rest.trim().strip_prefix('[')?.strip_suffix(']')?;
            Some((label.to_owned(), value::from_base58(id)?))
        })
        .collect()
}

/// A word as a shell reads it back: quoted when it holds anything but plain characters.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@=,+".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "
        const SEAT: PdaSeed = PdaSeed::new([5; 32]);
        enum Message { Open(u128), Mint { to: AccountId, amount: u128 } }
        lee_core::actor_entrypoint!(handle_message);
        fn handle_message(input: &ReceiveInput, message: Message) -> Response {
            match message {
                Message::Mint { to, amount } => { assert!(input.is_authorized); todo!() }
                _ => todo!(),
            }
        }
    ";

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn encodes_a_named_message_with_labels() {
        let interface = iface::parse(TOKEN).unwrap();
        let args = parse_args(words("--label alice=11111111111111111111111111111111").into_iter())
            .unwrap();
        let encoded = encode(&interface, &words("Mint to=alice amount=5"), &args).unwrap();
        assert_eq!(encoded.method.name, "Mint");
        assert_eq!(
            hex::encode(&encoded.message),
            format!("01{}05{}", "00".repeat(32), "00".repeat(15))
        );
        assert_eq!(encoded.mentioned, [[0; 32]]);
        assert_eq!(encoded.to_json()["hints"], json!(["signed"]));
        assert!(encode(&interface, &words("Burn 1"), &args).is_err());
    }

    #[test]
    fn flags_take_values_inline_or_next() {
        let args = parse_args(
            words("call x.rs --program P --to=pda:SEAT --actor A --actor B:sign --print Open 1")
                .into_iter(),
        )
        .unwrap();
        assert_eq!(args.program.as_deref(), Some("P"));
        assert_eq!(args.to.as_deref(), Some("pda:SEAT"));
        assert_eq!(args.actors, ["A", "B:sign"]);
        assert!(args.print);
        assert_eq!(args.words, ["call", "x.rs", "Open", "1"]);
        assert!(parse_args(words("iface x.rs --nope").into_iter()).is_err());
    }

    #[test]
    fn reads_wallet_labels_and_quotes_words() {
        let id = to_base58(&[3; 32]);
        let list = format!("Public/{id}\n/0 Public/{id} [alice]\n/ Private/{id} [secret]\n");
        assert_eq!(parse_account_list(&list), [("alice".to_owned(), [3; 32])]);
        assert_eq!(shell_word("Public/abc:sign"), "Public/abc:sign");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }
}
