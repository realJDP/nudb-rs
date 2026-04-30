use std::path::{Path, PathBuf};
use std::process::Command;

// These tests are intentionally ignored by default because they need a local
// checkout of upstream NuDB plus Boost headers/libs. Run with:
//
// NUDB_CPP_INCLUDE=/path/to/NuDB/include cargo test --test cpp_compat -- --ignored

struct CppEnv {
    nudb_include: String,
    boost_include: String,
    boost_lib: String,
}

fn cpp_env() -> CppEnv {
    CppEnv {
        nudb_include: std::env::var("NUDB_CPP_INCLUDE").expect("set NUDB_CPP_INCLUDE"),
        boost_include: std::env::var("BOOST_INCLUDE")
            .unwrap_or_else(|_| "/opt/homebrew/include".to_string()),
        boost_lib: std::env::var("BOOST_LIB").unwrap_or_else(|_| "/opt/homebrew/lib".to_string()),
    }
}

fn compile_cpp(env: &CppEnv, source: &Path, bin: &Path) {
    let status = Command::new("c++")
        .arg("-std=c++11")
        .arg("-I")
        .arg(&env.nudb_include)
        .arg("-I")
        .arg(&env.boost_include)
        .arg(source)
        .arg("-o")
        .arg(bin)
        .arg("-L")
        .arg(&env.boost_lib)
        .arg("-lboost_thread")
        .status()
        .unwrap();
    assert!(status.success());
}

fn paths(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (dir.join("db.dat"), dir.join("db.key"), dir.join("db.log"))
}

#[test]
#[ignore]
fn cpp_writes_rust_reads_fixture() {
    let env = cpp_env();
    let dir = tempfile::tempdir().unwrap();
    let cpp = dir.path().join("fixture.cpp");
    std::fs::write(
        &cpp,
        r#"
#include <nudb/nudb.hpp>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>
static void make_key(std::uint64_t i, unsigned char* k) {
    std::uint64_t x = i * 0x9e3779b97f4a7c15ULL;
    for(int n = 7; n >= 0; --n) { k[n] = static_cast<unsigned char>(x); x >>= 8; }
}
int main(int argc, char** argv) {
    using namespace nudb;
    error_code ec;
    auto dat = std::string(argv[1]) + "/db.dat";
    auto key = std::string(argv[1]) + "/db.key";
    auto log = std::string(argv[1]) + "/db.log";
    create<xxhasher>(dat, key, log, 7, 13, 8, 512, 0.5f, ec);
    if(ec) return 10;
    store db;
    db.open(dat, key, log, ec);
    if(ec) return 11;
    for(std::uint64_t i = 0; i < 2000; ++i) {
        unsigned char k[8];
        make_key(i, k);
        char value[64];
        auto n = std::snprintf(value, sizeof(value), "cpp-value-%llu", (unsigned long long)i);
        db.insert(k, value, static_cast<std::size_t>(n), ec);
        if(ec) return 12;
    }
    db.close(ec);
    return ec ? 13 : 0;
}
"#,
    )
    .unwrap();
    let bin = dir.path().join("fixture");
    compile_cpp(&env, &cpp, &bin);
    assert!(
        Command::new(&bin)
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );

    let (dat, key, log) = paths(dir.path());
    let mut store = nudb_rs::Store::open(dat, key, log).unwrap();
    for i in 0..2_000u64 {
        assert_eq!(
            store
                .fetch(&i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes())
                .unwrap(),
            format!("cpp-value-{i}").into_bytes()
        );
    }
    assert_eq!(store.verify().unwrap(), 2_000);
}

#[test]
#[ignore]
fn rust_writes_cpp_reads_fixture() {
    let env = cpp_env();
    let dir = tempfile::tempdir().unwrap();
    let (dat, key, log) = paths(dir.path());
    nudb_rs::Store::create(
        &dat,
        &key,
        &log,
        nudb_rs::CreateOptions {
            appnum: 7,
            uid: 13,
            salt: 17,
            key_size: 8,
            block_size: 512,
            load_factor: 0.5,
        },
    )
    .unwrap();
    let mut store = nudb_rs::Store::open(&dat, &key, &log).unwrap();
    for i in 0..2_000u64 {
        let k = i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes();
        store
            .insert(&k, format!("rust-value-{i}").as_bytes())
            .unwrap();
        if i % 97 == 0 {
            store.flush().unwrap();
        }
    }
    store.close().unwrap();

    let cpp = dir.path().join("reader.cpp");
    std::fs::write(
        &cpp,
        r#"
#include <nudb/nudb.hpp>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>
static void make_key(std::uint64_t i, unsigned char* k) {
    std::uint64_t x = i * 0x9e3779b97f4a7c15ULL;
    for(int n = 7; n >= 0; --n) { k[n] = static_cast<unsigned char>(x); x >>= 8; }
}
int main(int argc, char** argv) {
    using namespace nudb;
    error_code ec;
    auto dat = std::string(argv[1]) + "/db.dat";
    auto key = std::string(argv[1]) + "/db.key";
    auto log = std::string(argv[1]) + "/db.log";
    store db;
    db.open(dat, key, log, ec);
    if(ec) return 20;
    for(std::uint64_t i = 0; i < 2000; ++i) {
        unsigned char k[8];
        make_key(i, k);
        char expected[64];
        auto n = std::snprintf(expected, sizeof(expected), "rust-value-%llu", (unsigned long long)i);
        bool ok = false;
        db.fetch(k, [&](void const* data, std::size_t size) {
            ok = size == static_cast<std::size_t>(n) && std::memcmp(data, expected, size) == 0;
        }, ec);
        if(ec || !ok) return 21;
    }
    db.close(ec);
    return ec ? 22 : 0;
}
"#,
    )
    .unwrap();
    let bin = dir.path().join("reader");
    compile_cpp(&env, &cpp, &bin);
    assert!(
        Command::new(&bin)
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
}
