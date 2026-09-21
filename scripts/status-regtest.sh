#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/compose.sh"

assert_regtest_only

compose ps
