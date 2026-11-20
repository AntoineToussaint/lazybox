#!/usr/bin/env bash
# Exercise the candidate via Homebrew before its GitHub release URL exists.
# Run on a clean test machine, with the release directory as the sole argument.
set -euo pipefail
dist="$(cd "${1:?supply the release directory}" && pwd)"
tap=lazybox-test/candidate
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1

brew tap-new --no-git "$tap"
tapdir="$(brew --repository "$tap")"
case "$(uname -s)" in
  Linux)
    package="$tap/lazybox-runner"
    cp "$dist/lazybox-runner.rb" "$tapdir/Formula/"
    case "$(uname -m)" in
      aarch64) arch=arm64 ;;
      x86_64) arch=x64 ;;
      *) exit 1 ;;
    esac
    assets=("$dist"/lazybox-runner-*-linux-"$arch".tar.gz)
    flags=(--formula)
    ;;
  Darwin)
    package="$tap/lazybox-v2"
    mkdir -p "$tapdir/Casks"
    cp "$dist/lazybox-v2.rb" "$tapdir/Casks/"
    assets=("$dist"/Lazybox_*_aarch64.dmg)
    flags=(--cask)
    ;;
  *) exit 1 ;;
esac
[[ ${#assets[@]} == 1 && -f "${assets[0]}" ]]
brew trust "${flags[@]}" "$package"
cache="$(brew --cache "${flags[@]}" "$package")"
mkdir -p "$(dirname "$cache")"
cp "${assets[0]}" "$cache"
brew install "${flags[@]}" "$package"
if [[ "$(uname -s)" == Linux ]]; then
  brew test "$package"
  lazybox --version
else
  "/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox" --version
fi
brew uninstall "${flags[@]}" "$package"
