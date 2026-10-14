#!/bin/sh
# Rebuilds the three testlib_noauto* fixtures from testlib_noauto.s with
# PhxAss run under volamos itself (no vasm required). Run from the repo
# root after `cargo build`; PHXASS defaults to ~/amiga/PhxAss/PhxAss.
set -e
PHXASS="${PHXASS:-$HOME/amiga/PhxAss/PhxAss}"
build() {
    target/debug/volamos --cwd work: -V work:fixtures "$PHXASS" \
        work:testlib_noauto.s SET VARIANT="$1" TO "work:$2" | grep -E "error|No errors|Code:"
}
build 0 testlib_noauto
build 1 testlib_noauto_nullret
build 2 testlib_noauto_refuse
build 3 testlib_noauto_keep
