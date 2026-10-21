#!/bin/sh
# Build macswapi from source and install it.
#
#   ./install.sh                     install to /usr/local/bin (sudo if needed)
#   ./install.sh --prefix ~/.local   install to ~/.local/bin
#   ./install.sh --uninstall         remove it (pass the same --prefix)
set -eu

prefix=/usr/local
action=install

while [ $# -gt 0 ]; do
    case $1 in
        --prefix)
            [ $# -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 2; }
            prefix=$2
            shift 2
            ;;
        --prefix=*)
            prefix=${1#--prefix=}
            shift
            ;;
        --uninstall)
            action=uninstall
            shift
            ;;
        -h | --help)
            sed -n '2,6s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1 (see --help)" >&2
            exit 2
            ;;
    esac
done

# The shell doesn't expand ~ in --prefix=~/x; also make the prefix absolute.
case $prefix in
    "~") prefix=$HOME ;;
    "~/"*) prefix=$HOME/${prefix#"~/"} ;;
esac
case $prefix in
    /*) ;;
    *) prefix=$(pwd)/$prefix ;;
esac
bindir=${prefix%/}/bin
dest=$bindir/macswapi

# Use sudo only if the nearest existing ancestor of $bindir isn't writable.
pick_sudo() {
    d=$bindir
    while [ ! -e "$d" ]; do d=$(dirname "$d"); done
    if [ -w "$d" ]; then
        sudo=
    else
        echo "$d is not writable; using sudo"
        sudo=sudo
    fi
}

if [ "$action" = uninstall ]; then
    if [ ! -e "$dest" ]; then
        echo "nothing to remove: $dest not found"
        exit 0
    fi
    pick_sudo
    $sudo rm -f "$dest"
    echo "removed $dest"
    exit 0
fi

[ "$(uname -s)" = Darwin ] || { echo "macswapi only runs on macOS" >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || {
    echo "cargo not found: install Rust from https://rustup.rs or with 'brew install rust'" >&2
    exit 1
}

repo=$(cd "$(dirname "$0")" && pwd)
cargo build --release --locked --manifest-path "$repo/Cargo.toml"
built=${CARGO_TARGET_DIR:-$repo/target}/release/macswapi

pick_sudo
$sudo mkdir -p "$bindir"
$sudo install -m 755 "$built" "$dest"
echo "installed $dest"

case ":$PATH:" in
    *":$bindir:"*)
        found=$(command -v macswapi)
        if [ "$found" != "$dest" ]; then
            echo "note: 'macswapi' runs $found, which comes earlier in your PATH"
        fi
        ;;
    *) echo "note: $bindir is not on your PATH" ;;
esac
