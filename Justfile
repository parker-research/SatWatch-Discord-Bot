default:
    just --list

check:
    cargo fmt --all #-- --check
    cargo test --all-features --all-targets
    cargo clippy --all-features --all-targets -- -D warnings --no-deps
