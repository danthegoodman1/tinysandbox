#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
compose_file="$repo_dir/tests/s3/compose.yaml"
user_slug="$(printf '%s' "${USER:-user}" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9-' '-' | cut -c1-20)"
project="tinysandbox-s3-${user_slug}-$$"

port="${TINYSANDBOX_S3_TEST_PORT:-19000}"
if ! [[ "$port" =~ ^[0-9]{1,5}$ ]]; then
  echo "TINYSANDBOX_S3_TEST_PORT must be a decimal integer from 1 through 65535" >&2
  exit 2
fi
port_number=$((10#$port))
if ((port_number < 1 || port_number > 65535)); then
  echo "TINYSANDBOX_S3_TEST_PORT must be a decimal integer from 1 through 65535" >&2
  exit 2
fi

export TINYSANDBOX_S3_TEST_PORT="$port_number"
export TINYSANDBOX_S3_TEST_ENDPOINT="http://127.0.0.1:${TINYSANDBOX_S3_TEST_PORT}"
export TINYSANDBOX_S3_TEST_REGION="us-east-1"
export TINYSANDBOX_S3_TEST_ACCESS_KEY="tinysandbox-test"
export TINYSANDBOX_S3_TEST_SECRET_KEY="tinysandbox-test-secret"
export TINYSANDBOX_S3_TEST_BUCKET="tinysandbox-${project}"
export TINYSANDBOX_S3_TEST_PREFIX="fixtures/${project}/root"
export AWS_REGION="$TINYSANDBOX_S3_TEST_REGION"
export AWS_ACCESS_KEY_ID="$TINYSANDBOX_S3_TEST_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$TINYSANDBOX_S3_TEST_SECRET_KEY"
export AWS_EC2_METADATA_DISABLED="true"

cleanup() {
  docker compose --project-name "$project" --file "$compose_file" down --volumes --remove-orphans
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker compose --project-name "$project" --file "$compose_file" up --detach --wait --wait-timeout 90

cd "$repo_dir"
cargo test --locked --features s3 --test vfs_s3 -- --ignored --nocapture --test-threads=1
npm --prefix "$repo_dir/tinysandbox-node" run build
npm --prefix "$repo_dir/tinysandbox-node" run test:s3-compat
