#!/bin/sh
# Build macswapi from source and install it.
#
#   ./install.sh                     install to /usr/local/bin (sudo if needed)
#   ./install.sh --prefix ~/.local   install to ~/.local/bin
#   ./install.sh --uninstall         remove it (pass the same --prefix)
#   ./install.sh --no-completions    skip the zsh/bash/fish completions
set -eu

prefix=/usr/local
action=install
completions=yes

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
        --no-completions)
            completions=no
            shift
            ;;
        -h | --help)
            sed -n '2,7s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1 (see --help)" >&2
            exit 2
            ;;
    esac
done

# The shell doesn't expand ~ in --prefix=~/x; also make the prefix absolute.
# shellcheck disable=SC2088 # literal "~/" match is intentional
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
share=${prefix%/}/share
zsh_comp=$share/zsh/site-functions/_macswapi
bash_comp=$share/bash-completion/completions/macswapi
fish_comp=$share/fish/vendor_completions.d/macswapi.fish

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
    for f in "$zsh_comp" "$bash_comp" "$fish_comp"; do
        if [ -e "$f" ]; then
            $sudo rm -f "$f"
            echo "removed $f"
        fi
    done
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

if [ "$completions" = yes ]; then
    for shell in zsh bash fish; do
        case $shell in
            zsh) f=$zsh_comp ;;
            bash) f=$bash_comp ;;
            fish) f=$fish_comp ;;
        esac
        tmp=$(mktemp)
        "$built" --completions "$shell" > "$tmp"
        $sudo mkdir -p "$(dirname "$f")"
        $sudo install -m 644 "$tmp" "$f"
        rm -f "$tmp"
    done
    echo "installed completions under $share (zsh, bash, fish)"
    case $prefix in
        /usr/local | /opt/homebrew) ;;
        *) echo "note: for zsh, add $share/zsh/site-functions to fpath before compinit" ;;
    esac
fi

case ":$PATH:" in
    *":$bindir:"*)
        found=$(command -v macswapi)
        if [ "$found" != "$dest" ]; then
            echo "note: 'macswapi' runs $found, which comes earlier in your PATH"
        fi
        ;;
    *) echo "note: $bindir is not on your PATH" ;;
esac
