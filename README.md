Install Rust: <https://rust-lang.org/tools/install/>

To test:

    RUST_BACKTRACE=1 cargo test

With coverage:

    cargo llvm-cov --show-missing-lines
