# lez-call

Call any [LEZ](https://github.com/logos-blockchain/logos-execution-zone) program from its guest's source.

A built guest carries no description of its messages: Borsh encodes an enum variant as its index and its fields, with no names. So `lez-call` reads the guest's source:
- `iface` lists the messages its handler takes and their params in Borsh order;
- `encode` encodes one message from typed values;
- `call` sends it with the wallet's `wallet program call`.

It doesn't read the chain and doesn't link any LEZ crate.

```
lez-call iface  <guest.rs>
lez-call encode <guest.rs> [<Method>] [values]…
lez-call call   <guest.rs> --program <id> --to <actor> [--actor <actor>]… [--payer <account>]
                [--print] [<Method>] [values]…
```

## Requirements

- **The wallet:** `call`, and label lookups, run the `wallet` CLI from LEZ: `$LEZ_WALLET`, or `wallet` on `PATH`. It must have the `program call` subcommand and is run with this environment (`LEE_WALLET_HOME_DIR`, `LEE_WALLET_PASSWORD`).
- **A sequencer:** whatever the wallet is configured for.

## Examples

```sh
G=../logos-execution-zone/examples/program_deployment/methods/guest/src/bin

lez-call iface $G/hello_world_with_move_function.rs
lez-call encode $G/hello_world.rs "Hola"                                   # 0x04000000486f6c61
lez-call --json encode token.rs Mint to=alice amount=30                    # LEZCALL {"message_hex":…}
lez-call call $G/tail_call_with_pda.rs --program $PROGRAM --to pda:PDA_SEED --actor "$ACCOUNT@$PROGRAM" "$CALLEE"
lez-call call token.rs --program $PROGRAM --to "Public/$SEAT:sign" --print Mint to=alice amount=30
```

## Values and actors

- **Values:** a message is its variant's name (or index) and its values, as `name=value` or in order. A program with one message type takes just its values.
- **Local types:** a local struct is expanded into its fields (`range.start`, `range.end`), and a local enum whose variants are all units takes a variant's name.
- **Value formats:**

| Type | Value |
|---|---|
| `Vec<u8>`, `String` | text, or `0x…` hex for `Vec<u8>` |
| integers | decimal |
| `bool` | `true` / `false` |
| `AccountId`, `PdaSeed` | base58, `Public/<base58>`, `0x…` hex, or a wallet label |
| `Actor` | `account@program` |
| `Option<T>` | empty or `none`, or a `T` |
| `Vec<T>` | comma-separated `T`s |
| `[u8; N]`, anything else | `0x…` hex of its Borsh encoding |

- **Labels:** labels come from `--label <name>=<id>` or, failing that, from `wallet account list`.
- **Actors:** an actor is what `wallet program call` takes:
  - `<account>[@program][:sign]`;
  - `pda:[<owner>/]<seed>[@program]` for a public PDA;
  - `pk:<hex>[@program]` for an account known by its public key.

  The program defaults to `--program`. In `pda:`, the seed may also be the name of a `PdaSeed` constant in the source: `lez-call` puts its bytes in.

## Output

`--json` prints one `LEZCALL {json}` line:
- **`iface`:** `{handler, message, methods: [{name, index, fields, params: [{name, ty, hint, choices?}], doc, hints}], seeds: [{name, bytes}], source}`
- **`encode`:** `{method, index, hints, message_hex, mentioned}`. `mentioned` is the account ids its values name.
- **`call --print`:** the same, plus `command` and `wallet_args`.

Hints come from what each message's match arm checks. `signed` means it checks `is_authorized`; `internal` means it checks `from`, so another program sends it. They're labels only: whether a call goes through is the chain's to decide.

## Dependencies

`syn`, `quote`, `serde`, `serde_json`, `bs58` and `anyhow`: 14 crates in all. Arguments, hex and Borsh are written by hand.
