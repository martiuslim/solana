#!/bin/bash -e

cd "$(dirname "$0")/.."

export RUST_BACKTRACE=1
rustc --version
cargo --version

_() {
  echo "--- $*"
  "$@"
}

_ cargo build --verbose --features unstable
_ cargo test --verbose --features unstable
_ cargo clippy -- --deny=warnings

exit 0

# Coverage disabled (see issue #433)
_ cargo cov test
_ cargo cov report

echo --- Coverage report:
ls -l target/cov/report/index.html

if [[ -z "$CODECOV_TOKEN" ]]; then
  echo CODECOV_TOKEN undefined
else
  bash <(curl -s https://codecov.io/bash) -x 'llvm-cov-6.0 gcov'
fi

