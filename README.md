# nudb-rs

A compatibility-first Rust port of NuDB.

This project treats upstream C++ NuDB as the oracle. The goal is not a
NuDB-inspired store; it is byte-compatible `.dat`, `.key`, and `.log` handling
with tests that prove interoperability in both directions.

The implementation follows the upstream C++ NuDB layout:

- `.dat`: append-only value and spill records
- `.key`: fixed-size linear-hash buckets
- `.log`: rollback records for crash recovery
- big-endian integer fields, including NuDB's 48-bit fields
- salted xxHash64 with NuDB's upper-48-bit hash reduction

Current API:

- create/open a store
- insert fixed-size keys and non-empty values
- fetch by key
- flush/close
- visit the data file
- verify that all data-file values are reachable through the key file
- recover from a present NuDB log file on open

## Compatibility Status

Proven by tests:

- Rust creates, inserts, fetches, flushes, reopens, visits, and verifies stores
- deterministic 2,500-record stress with many flushes
- rollback recovery from a present log header
- rollback recovery restoring a logged key bucket after deliberate key-file corruption
- C++ NuDB writes a 2,000-record fixture and Rust reads/verifies it
- Rust writes a 2,000-record fixture with periodic flushes and C++ NuDB reads it

Still intentionally future work:

- async/background commit thread matching upstream's runtime behavior
- API-level concurrent fetch ergonomics
- offline key-file rebuild/rekey tooling
- very large multi-million-record soak runs
- real rippled node-store fixtures

The on-disk format and core algorithms are now covered by both native Rust tests
and upstream C++ oracle tests.

## Testing

Run the Rust tests:

```sh
cargo test
```

Run the upstream C++ oracle tests when NuDB headers are available:

```sh
NUDB_CPP_INCLUDE=/path/to/NuDB/include cargo test --test cpp_compat -- --ignored
```

On Homebrew macOS, Boost is found automatically under `/opt/homebrew`. Override
with `BOOST_INCLUDE` and `BOOST_LIB` when needed:

```sh
BOOST_INCLUDE=/path/to/include \
BOOST_LIB=/path/to/lib \
NUDB_CPP_INCLUDE=/path/to/NuDB/include \
cargo test --test cpp_compat -- --ignored
```

The oracle suite tests both directions: C++ writes/Rust reads and Rust
writes/C++ reads.

## Example

```rust
use nudb_rs::{CreateOptions, Store};

let dat = "db.dat";
let key = "db.key";
let log = "db.log";

Store::create(dat, key, log, CreateOptions::new(1, 32, 4096))?;

let mut store = Store::open(dat, key, log)?;
let key_bytes = [7u8; 32];
store.insert(&key_bytes, b"value")?;
store.flush()?;

assert_eq!(store.fetch(&key_bytes)?, b"value");
# Ok::<(), nudb_rs::Error>(())
```

