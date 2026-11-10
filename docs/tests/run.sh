#!/usr/bin/env bash
# Run the documentation acceptance checks: link resolution and the command
# surface. Both are offline and read only repository files.
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

bash "$here/check-links.sh"
bash "$here/check-commands.sh"

printf 'PASS  docs: link check and command-exists check passed\n'
