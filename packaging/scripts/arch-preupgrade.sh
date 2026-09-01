#!/bin/sh
set -eu
# Bitty packaging hook - bounded, no network, no unsafe.
# Keep empty: Bitty has no daemon, no system-wide config migration at 0.0.1.
# This script exists to satisfy nfpm overrides and to document explicit no-op.
exit 0
