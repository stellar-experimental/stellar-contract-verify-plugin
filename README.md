# stellar-contract-verify

> [!IMPORTANT]
> 
> This project should not be used in production. It's just meant for 
> experimentation and testing. It is not maintained and may be removed at any 
> time.

A [Stellar CLI plugin](https://developers.stellar.org/docs/tools/cli/plugins)
that verifies a Stellar contract's WASM reproduces from the build metadata it
records, per [SEP-58](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0058.md).

It reads the WASM's `contractmetav0` section, pulls the recorded build image
(`bldimg`), materializes the recorded source archive, rebuilds the contract
inside that pinned image, and byte-compares the result against the original.

## How it works as a plugin

The binary is named `stellar-contract-verify`. When it is on your `PATH`, the
Stellar CLI's plugin fallback runs it whenever you invoke:

```
stellar contract verify --wasm ./my_contract.wasm
```

You can also run it directly:

```
stellar-contract-verify --wasm ./my_contract.wasm
```

## Requirements

- A container engine on your `PATH` — `docker` (default) or Apple's `container`
  CLI (`--engine apple-container`) — to run the recorded, digest-pinned image.
- The `stellar` CLI on your `PATH`: it's used to resolve the data directory
  (`stellar cache path`) and, with `--id` / `--wasm-hash`, to fetch the WASM
  (`stellar contract fetch`).

## Usage

Pass exactly one WASM source: a local `--wasm` file, or a network `--id` /
`--wasm-hash`.

```
stellar-contract-verify [OPTIONS] (--wasm <WASM> | --id <ID> | --wasm-hash <HASH>)

Options:
      --wasm <WASM>              Local WASM file to verify
      --id <ID>                  Contract id or alias to fetch the WASM from the network
      --wasm-hash <HASH>         WASM hash (hex) to fetch the WASM from the network
  -n, --network <NETWORK>        Named network to fetch from (e.g. testnet); only used
                                 with --id / --wasm-hash
      --source-uri <SOURCE_URI>  Source archive (http(s) URL or local path) to use
                                 when the WASM records only `source_sha256`, or to
                                 override the recorded `source_uri`
      --trust                    Skip interactive trust confirmation for a non-default
                                 build image or for the source archive
      --keep                     Keep the materialized source + rebuilt WASM and print
                                 their paths (useful for debugging a byte mismatch)
      --quiet                    Only print the final verdict
  -v, --verbose                  Print the container command and stream build output
      --engine <ENGINE>          Container engine: docker (default) or apple-container
  -d, --docker-host <HOST>       Override the docker host (docker engine only)
      --cpus <CPUS>              Limit CPUs for the build container (whole number)
      --memory <MEMORY>          Limit memory for the build container, e.g. 2g / 512m
```

On success it prints `Verified: <n> bytes, sha256=<hash>` and exits `0`; on a
byte mismatch it reports both hashes/sizes and exits non-zero.

### Fetching from the network

`--id` / `--wasm-hash` shell out to `stellar contract fetch --network <name>`,
so network resolution, aliases, and RPC access all match the CLI you already
have configured. `--network` is forwarded as-is; if you omit it, the CLI's
configured default network applies.

### Trust

Only `docker.io/stellar/stellar-cli@sha256:…` is trusted by default. Any other
build image, and every source archive, requires interactive confirmation (or
`--trust` to bypass). In a non-interactive context, an untrusted value fails
unless `--trust` is passed.

## Installation

Install straight from the git repo with cargo:

```
cargo install --git https://github.com/stellar-experimental/stellar-contract-verify-plugin
```

Make sure your cargo's bin directory is on your `PATH` so the Stellar CLI can
find the installed `stellar-contract-verify` binary — then `stellar contract
verify` just works.

Verify it's picked up:

```
stellar plugin ls        # should list `contract-verify`
stellar contract verify --help
```
