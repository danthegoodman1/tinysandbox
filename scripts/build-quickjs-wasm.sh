#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${ROOT}/assets/quickjs.wasm"
WORK="${THINBOX_QUICKJS_BUILD_DIR:-${ROOT}/target/quickjs-wasm-build}"

QUICKJS_REPO="https://github.com/quickjs-ng/quickjs.git"
QUICKJS_TAG="v0.15.1"
QUICKJS_COMMIT="fd0a0210b7be00957751871e7e01b8291268fc29"
WASI_SDK_VERSION="27"
WASI_SDK_VERSION_FULL="${WASI_SDK_VERSION}.0"

case "$(uname -s)" in
  Darwin) WASI_OS="macos" ;;
  Linux) WASI_OS="linux" ;;
  *) echo "unsupported host OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) WASI_ARCH="arm64" ;;
  x86_64|amd64) WASI_ARCH="x86_64" ;;
  *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "${WORK}" "$(dirname "${OUT}")"

SDK_DIR="${WORK}/wasi-sdk-${WASI_SDK_VERSION_FULL}-${WASI_ARCH}-${WASI_OS}"
if [[ ! -x "${SDK_DIR}/bin/clang" ]]; then
  SDK_TARBALL="${WORK}/wasi-sdk.tar.gz"
  SDK_URL="https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/wasi-sdk-${WASI_SDK_VERSION_FULL}-${WASI_ARCH}-${WASI_OS}.tar.gz"
  curl -L "${SDK_URL}" -o "${SDK_TARBALL}"
  tar -xzf "${SDK_TARBALL}" -C "${WORK}"
fi

SRC_DIR="${WORK}/quickjs"
if [[ ! -d "${SRC_DIR}/.git" ]]; then
  git clone --depth 1 --branch "${QUICKJS_TAG}" "${QUICKJS_REPO}" "${SRC_DIR}"
fi
ACTUAL_COMMIT="$(git -C "${SRC_DIR}" rev-parse HEAD)"
if [[ "${ACTUAL_COMMIT}" != "${QUICKJS_COMMIT}" ]]; then
  echo "quickjs-ng commit mismatch: expected ${QUICKJS_COMMIT}, got ${ACTUAL_COMMIT}" >&2
  exit 1
fi

python3 - "${SRC_DIR}/quickjs.c" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
old = "#if defined(__wasi__)\n    rt->stack_limit = 0; /* no limit */\n#else\n"
new = "#if defined(__wasi__) && !defined(THINBOX_WASI_STACK_LIMIT)\n    rt->stack_limit = 0; /* no limit */\n#else\n"
if old in text:
    path.write_text(text.replace(old, new, 1))
elif new not in text:
    raise SystemExit("quickjs.c stack-limit patch target not found")
PY

CC="${SDK_DIR}/bin/clang"
STRIP="${SDK_DIR}/bin/llvm-strip"
TMP_OUT="${WORK}/quickjs-thinbox.wasm"

"${CC}" \
  --target=wasm32-wasip1 \
  -mexec-model=reactor \
  -Oz \
  -DNDEBUG \
  -D_GNU_SOURCE \
  -DTHINBOX_WASI_STACK_LIMIT \
  -I"${SRC_DIR}" \
  "${ROOT}/src/js/quickjs_shim.c" \
  "${SRC_DIR}/quickjs.c" \
  "${SRC_DIR}/dtoa.c" \
  "${SRC_DIR}/libregexp.c" \
  "${SRC_DIR}/libunicode.c" \
  -Wl,--allow-undefined \
  -Wl,-z,stack-size=4194304 \
  -Wl,--export=thinbox_alloc \
  -Wl,--export=thinbox_free \
  -Wl,--export=thinbox_run \
  -Wl,--export=memory \
  -Wl,--strip-all \
  -o "${TMP_OUT}"

"${STRIP}" "${TMP_OUT}" -o "${OUT}"
ls -lh "${OUT}"
