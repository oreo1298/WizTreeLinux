#!/usr/bin/env bash
# Install, update or uninstall Dendro.
#
#   ./install.sh               install (asks before installing anything)
#   ./install.sh update        download the latest version and reinstall it
#   ./install.sh uninstall     remove Dendro
#
# Options:
#   --user          install for your user only (~/.local), no sudo for the install step
#   --prefix DIR    install to DIR instead of /usr/local
#   --no-package    on Arch-based systems, don't build a pacman package
#   --no-deps       don't install build dependencies
#   -y, --yes       don't ask for confirmation
#   -h, --help      show this help
#
# On Arch Linux and derivatives the script builds a regular pacman package
# (packaging/arch/PKGBUILD), so pacman keeps track of every installed file.
# Elsewhere it builds with cargo and installs with `make install`.

set -euo pipefail

APP_ID=io.github.oreo1298.Dendro
MIN_RUST=1.85
MIN_GTK=4.12
MIN_ADW=1.5

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
STATE_FILE="${XDG_STATE_HOME:-$HOME/.local/state}/dendro/install"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/dendro"

action=install
user_install=0
prefix=
yes=0
deps=1
use_pkg=auto

if [[ -t 1 ]]; then
  B=$'\e[1m' BLUE=$'\e[1;34m' YEL=$'\e[1;33m' RED=$'\e[1;31m' GRN=$'\e[1;32m' R=$'\e[0m'
else
  B='' BLUE='' YEL='' RED='' GRN='' R=''
fi

step() { printf '%s==>%s %s%s%s\n' "$BLUE" "$R" "$B" "$*" "$R"; }
note() { printf '    %s\n' "$*"; }
ok() { printf '%s==>%s %s%s%s\n' "$GRN" "$R" "$B" "$*" "$R"; }
warn() { printf '%swarning:%s %s\n' "$YEL" "$R" "$*" >&2; }
die() { printf '%serror:%s %s\n' "$RED" "$R" "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() { sed -n '2,19p' "$HERE/install.sh" | sed 's/^# \{0,1\}//'; }

ask() {
  [[ $yes == 1 ]] && return 0
  local answer
  read -r -p "$1 [Y/n] " answer </dev/tty || return 1
  [[ -z $answer || $answer == [Yy]* ]]
}

as_root() {
  if [[ $EUID -eq 0 ]]; then
    "$@"
  elif have sudo; then
    sudo "$@"
  elif have doas; then
    doas "$@"
  else
    die "This step needs administrator rights, but neither sudo nor doas is available."
  fi
}

# version_ge A B: succeeds if version A >= version B.
version_ge() {
  [[ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

distro_family() {
  local id="" like=""
  if [[ -r /etc/os-release ]]; then
    # shellcheck disable=SC1091
    id=$(. /etc/os-release && echo "${ID:-}")
    # shellcheck disable=SC1091
    like=$(. /etc/os-release && echo "${ID_LIKE:-}")
  fi
  local x
  for x in $id $like; do
    case $x in
      arch | archarm | manjaro | endeavouros | garuda | artix | cachyos | arcolinux) echo arch; return ;;
      debian | ubuntu | linuxmint | pop | elementary | zorin | neon | kali | raspbian | deepin) echo debian; return ;;
      fedora | rhel | centos | rocky | almalinux | nobara | ultramarine) echo fedora; return ;;
      opensuse* | suse | sles) echo suse; return ;;
      void) echo void; return ;;
      alpine | postmarketos) echo alpine; return ;;
      gentoo) echo gentoo; return ;;
      nixos) echo nixos; return ;;
    esac
  done
  echo unknown
}

install_deps() {
  local family=$1 pkgs=()
  case $family in
    arch)
      pkgs=(base-devel git pkgconf gtk4 libadwaita)
      have cargo || pkgs+=(rust)
      step "Installing build dependencies with pacman"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      if [[ $yes == 1 ]]; then as_root pacman -S --needed --noconfirm "${pkgs[@]}"; else as_root pacman -S --needed "${pkgs[@]}"; fi
      ;;
    debian)
      # The distribution's Rust is often too old; rustup is used if needed.
      pkgs=(build-essential pkg-config git curl libgtk-4-dev libadwaita-1-dev)
      step "Installing build dependencies with apt"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root apt-get update
      as_root apt-get install -y "${pkgs[@]}"
      ;;
    fedora)
      pkgs=(gcc make pkgconf-pkg-config git gtk4-devel libadwaita-devel)
      have cargo || pkgs+=(cargo rust)
      step "Installing build dependencies with dnf"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root dnf install -y "${pkgs[@]}"
      ;;
    suse)
      pkgs=(gcc make pkgconf git gtk4-devel libadwaita-devel)
      have cargo || pkgs+=(cargo rust)
      step "Installing build dependencies with zypper"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root zypper --non-interactive install "${pkgs[@]}"
      ;;
    void)
      pkgs=(base-devel pkg-config git gtk4-devel libadwaita-devel)
      have cargo || pkgs+=(cargo rust)
      step "Installing build dependencies with xbps"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root xbps-install -Sy "${pkgs[@]}"
      ;;
    alpine)
      pkgs=(build-base pkgconf git gtk4.0-dev libadwaita-dev)
      have cargo || pkgs+=(cargo rust)
      step "Installing build dependencies with apk"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root apk add "${pkgs[@]}"
      ;;
    gentoo)
      pkgs=(gui-libs/gtk gui-libs/libadwaita dev-vcs/git virtual/pkgconfig)
      have cargo || pkgs+=(dev-lang/rust-bin)
      step "Installing build dependencies with emerge"
      note "${pkgs[*]}"
      ask "Continue?" || die "Cancelled."
      as_root emerge --noreplace "${pkgs[@]}"
      ;;
    nixos)
      die "On NixOS, build inside a shell with the dependencies:
    nix-shell -p cargo rustc pkg-config gtk4 libadwaita --run 'make'
and add the result to your configuration, or use: ./install.sh --user --no-deps from that shell."
      ;;
    *)
      warn "Unknown distribution. Please install these yourself, then run: ./install.sh --no-deps"
      note "- a C compiler, make, pkg-config and git"
      note "- GTK $MIN_GTK or newer, with development files"
      note "- libadwaita $MIN_ADW or newer, with development files"
      note "- Rust $MIN_RUST or newer (https://rustup.rs)"
      exit 1
      ;;
  esac
}

cargo_version() { cargo --version 2>/dev/null | awk '{print $2}'; }

ensure_rust() {
  # A rustup installation may not be on PATH in this shell yet.
  # shellcheck disable=SC1091
  [[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
  if have cargo && version_ge "$(cargo_version)" "$MIN_RUST"; then
    return
  fi
  if have cargo; then
    warn "Rust $(cargo_version) is too old; Dendro needs Rust $MIN_RUST or newer."
  else
    warn "Rust is not installed."
  fi
  if have rustup; then
    step "Updating Rust with rustup"
    rustup toolchain install stable --profile minimal
    export RUSTUP_TOOLCHAIN=stable
  else
    ask "Install the latest Rust for your user with rustup (https://rustup.rs)?" ||
      die "Rust $MIN_RUST or newer is required."
    have curl || die "curl is needed to download rustup."
    step "Installing Rust with rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  version_ge "$(cargo_version)" "$MIN_RUST" || die "Could not get Rust $MIN_RUST or newer."
}

check_libs() {
  have pkg-config || die "pkg-config is missing. Install it, or run ./install.sh without --no-deps."
  local name ver min pc
  for pc in "gtk4:GTK:$MIN_GTK" "libadwaita-1:libadwaita:$MIN_ADW"; do
    IFS=: read -r name ver min <<<"$pc"
    if ! pkg-config --exists "$name"; then
      die "$ver development files were not found. Run ./install.sh without --no-deps, or install them manually."
    fi
    if ! pkg-config --atleast-version="$min" "$name"; then
      die "Your $ver is version $(pkg-config --modversion "$name"), but Dendro needs $min or newer.
    Distributions that are new enough include Arch, Fedora 40+, Ubuntu 24.04+, Debian 13+,
    Linux Mint 22+, openSUSE Tumbleweed and Leap 16."
    fi
  done
}

save_state() {
  mkdir -p "$(dirname "$STATE_FILE")"
  printf 'inst_method=%q\ninst_prefix=%q\n' "$1" "${2:-}" >"$STATE_FILE"
}

detect_install() {
  inst_method=none
  inst_prefix=
  if have pacman && pacman -Qq dendro >/dev/null 2>&1; then
    inst_method=pkg
    return
  fi
  if [[ -r $STATE_FILE ]]; then
    # shellcheck disable=SC1090
    . "$STATE_FILE"
    if [[ $inst_method == make && -x $inst_prefix/bin/dendro ]]; then
      return
    fi
    inst_method=none
    inst_prefix=
  fi
  local p
  for p in /usr/local "$HOME/.local" /usr; do
    if [[ -x $p/bin/dendro && -f $p/share/applications/$APP_ID.desktop ]]; then
      inst_method='make'
      inst_prefix=$p
      return
    fi
  done
}

install_package() {
  have makepkg || die "makepkg was not found. Install base-devel, or use --no-package."
  step "Building and installing the pacman package"
  note "pacman will ask for your password to install it."
  local flags=(-s -i -f)
  [[ $yes == 1 ]] && flags+=(--noconfirm)
  (cd "$HERE/packaging/arch" && makepkg "${flags[@]}")
  save_state pkg ""
}

install_files() {
  step "Building Dendro (the first build takes a few minutes)"
  (cd "$HERE" && cargo build --release --locked)
  if [[ $user_install == 1 ]]; then
    prefix=${prefix:-$HOME/.local}
    step "Installing to $prefix"
    make -C "$HERE" install PREFIX="$prefix"
  else
    prefix=${prefix:-/usr/local}
    step "Installing to $prefix (needs administrator rights)"
    as_root make -C "$HERE" install PREFIX="$prefix"
  fi
  save_state make "$prefix"
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) note "Tip: add $prefix/bin to your PATH to start 'dendro' from a terminal." ;;
  esac
}

do_install() {
  local family
  family=$(distro_family)
  if [[ $EUID -eq 0 ]]; then
    warn "Running as root. It is better to run this script as your normal user;"
    warn "it asks for the administrator password only when needed."
  fi
  if [[ $use_pkg == auto ]]; then
    if [[ $family == arch && $user_install == 0 && -z $prefix && $EUID -ne 0 ]]; then
      use_pkg=1
    else
      use_pkg=0
    fi
  fi
  if [[ $deps == 1 ]]; then
    install_deps "$family"
  fi
  ensure_rust
  check_libs
  if [[ $use_pkg == 1 ]]; then
    install_package
  else
    install_files
  fi
  ok "Dendro is installed. Start it from your application menu or run: dendro"
}

do_update() {
  detect_install
  if [[ $inst_method == none ]]; then
    warn "Dendro does not seem to be installed yet; installing it now."
    do_install
    return
  fi
  if [[ -d $HERE/.git ]] && have git; then
    step "Downloading the latest version"
    git -C "$HERE" pull --ff-only ||
      die "Could not update the source code (do you have local changes?). Fix that and run ./install.sh update again."
  else
    warn "This folder is not a git checkout; rebuilding the code that is here."
  fi
  # Run the (possibly updated) installer for the reinstall.
  local args=(install --no-deps)
  [[ $yes == 1 ]] && args+=(--yes)
  if [[ $inst_method == pkg ]]; then
    :
  else
    args+=(--no-package --prefix "$inst_prefix")
    [[ $inst_prefix == "$HOME"/* ]] && args+=(--user)
  fi
  exec bash "$HERE/install.sh" "${args[@]}"
}

do_uninstall() {
  detect_install
  case $inst_method in
    pkg)
      step "Removing the dendro package"
      as_root pacman -R dendro
      ;;
    make)
      step "Removing Dendro from $inst_prefix"
      if [[ $inst_prefix == "$HOME"/* ]]; then
        make -C "$HERE" uninstall PREFIX="$inst_prefix"
      else
        as_root make -C "$HERE" uninstall PREFIX="$inst_prefix"
      fi
      ;;
    none)
      warn "Dendro does not seem to be installed."
      ;;
  esac
  rm -f "$STATE_FILE"
  if [[ -d $CONFIG_DIR ]] && ask "Also delete your Dendro settings ($CONFIG_DIR)?"; then
    rm -rf "$CONFIG_DIR"
  fi
  [[ $inst_method != none ]] && ok "Dendro was removed."
  return 0
}

main() {
  while [[ $# -gt 0 ]]; do
    case $1 in
      install | update | uninstall) action=$1 ;;
      --user) user_install=1 ;;
      --prefix) prefix=${2:?--prefix needs a directory}; shift ;;
      --prefix=*) prefix=${1#*=} ;;
      --no-package) use_pkg=0 ;;
      --no-deps) deps=0 ;;
      -y | --yes) yes=1 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "Unknown option: $1" ;;
    esac
    shift
  done
  if [[ -n $prefix ]]; then
    prefix=$(realpath -m "$prefix" 2>/dev/null || printf %s "$prefix")
  fi
  case $action in
    install) do_install ;;
    update) do_update ;;
    uninstall) do_uninstall ;;
  esac
}

# Everything is wrapped in functions so that `git pull` replacing this file
# during an update cannot confuse the running shell.
main "$@"; exit $?
