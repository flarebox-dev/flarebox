#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Builds the site for Cloudflare Pages: site/ plus the repository's install.sh, so
# https://koidrive.com/install.sh serves the same file as main. The Pages project runs
# it from the repository root with "sh scripts/site-build.sh" and publishes site-dist/.

set -eu

rm -rf site-dist
cp -R site site-dist
cp install.sh site-dist/install.sh
echo "Built site-dist/ with install.sh ($(wc -c <install.sh | tr -d ' ') bytes)."
