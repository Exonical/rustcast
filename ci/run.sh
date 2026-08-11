#!/usr/bin/env bash
set -euo pipefail

mkdir -p target/junit
rm -f target/junit/*.xml
suite="${1:-all}"

run_rust() {
  (cd flux && cargo nextest run --workspace --profile ci)
  cp flux/target/nextest/target/junit/rust.xml target/junit/rust.xml
  (cd flux && cargo clippy --workspace --all-targets)
}

run_windows() {
  cargo xwin check --manifest-path flux/Cargo.toml --target x86_64-pc-windows-msvc -p flux-server
}

run_go() {
  cargo test --manifest-path tools/go-junit/Cargo.toml
  set -o pipefail
  (cd flux-web && go test -json ./...) | cargo run --manifest-path tools/go-junit/Cargo.toml -- target/junit/go.xml
  test -z "$(cd flux-web && gofmt -l .)"
}

run_ui() {
  (cd flux-web/ui && pnpm install --frozen-lockfile && pnpm lint && pnpm test && pnpm run test:junit && pnpm build)
}

case "$suite" in
  rust) run_rust ;;
  windows) run_windows ;;
  go) run_go ;;
  ui) run_ui ;;
  all)
    run_rust
    run_go
    run_ui
    ;;
  *)
    echo "usage: $0 [all|rust|windows|go|ui]" >&2
    exit 2
    ;;
esac
