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

To run the example application, download the Yelp dataset from here:
<https://business.yelp.com/data/resources/open-dataset/>. Unzip and
untar the contents:

    unzip Yelp-JSON.zip
    cd Yelp-JSON
    tar xf yelp_dataset.tar

Call the program

    cargo run -- <path to>/yelp_academic_dataset_business.json
