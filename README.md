Install Rust: <https://rust-lang.org/tools/install/>

To test:

    RUST_BACKTRACE=1 cargo test

With coverage:

    cargo llvm-cov --show-missing-lines

See output in realtime (useful for debugging hangs)

     cargo test -- --nocapture

Use debugger:

    cargo test --no-run
    rust-lldb target/debug/deps/<executable name from above>
