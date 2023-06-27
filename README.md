nym-dumpster
============

A generic Rust async interface to `nym-client`.

## Usage

1. Start nym-client

```
$ git clone https://github.com/nymtech/nym
$ cd nym
$ git checkout v1.1.22
$ cargo build --release
$ cp target/release/nym-client .
$ ./nym-client init --id acab
$ ./nym-client run --id acab
```

2. Wait for https://github.com/nymtech/nym/issues/3610

3. Run the examples

```
$ cargo run --release --example server
$ cargo run --release --example client
```

## License

GNU AGPL 3.0
