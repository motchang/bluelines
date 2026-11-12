#!/bin/sh
# Regenerates assets/vendor from the npm packages pinned in package-lock.json
# next to this script.
#
# Each package tarball is downloaded from the URL the lockfile records and
# checked against the lockfile's sha512 integrity before anything is copied out
# of it. Nothing from the packages is executed, and their dependencies are not
# installed: the files copied are self-contained bundles.
#
#   packaging/vendor/update.sh          rewrite assets/vendor
#   packaging/vendor/update.sh --check  fail if assets/vendor differs
#
# Needs node (to read the lockfile), curl, openssl and tar.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
vendor=$(cd "$here/../../assets/vendor" && pwd)

check=false
case "${1-}" in
  --check) check=true ;;
  "") ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Prints "<resolved URL> <integrity>" for a direct dependency, after checking
# that the lockfile pins the version package.json asks for.
locked() {
  node -e '
    const [dir, name] = process.argv.slice(1);
    const manifest = require(dir + "/package.json");
    const entry = require(dir + "/package-lock.json").packages["node_modules/" + name];
    const wanted = manifest.dependencies[name];
    if (!entry || entry.version !== wanted) {
      console.error(`${name}: package-lock.json has ${entry && entry.version}, package.json wants ${wanted}; run npm install --package-lock-only`);
      process.exit(1);
    }
    console.log(entry.resolved, entry.integrity);
  ' "$here" "$1"
}

# Downloads, verifies and unpacks a package into $work/<dir>/package.
fetch() {
  name=$1
  dir=$2
  set -- $(locked "$name")
  url=$1
  integrity=$2
  case "$integrity" in
    sha512-*) ;;
    *) echo "$name: unexpected integrity $integrity" >&2; exit 1 ;;
  esac
  curl -sSfL -o "$work/$dir.tgz" "$url"
  actual="sha512-$(openssl dgst -sha512 -binary "$work/$dir.tgz" | openssl base64 -A)"
  if [ "$actual" != "$integrity" ]; then
    echo "$name: integrity mismatch for $url" >&2
    echo "  expected $integrity" >&2
    echo "  got      $actual" >&2
    exit 1
  fi
  mkdir "$work/$dir"
  tar -xzf "$work/$dir.tgz" -C "$work/$dir"
}

fetch github-markdown-css gmc
fetch @highlightjs/cdn-assets hljs
fetch mermaid mermaid

out="$work/out"
mkdir "$out"

gmc="$work/gmc/package"
cp "$gmc/github-markdown.css" "$out/github-markdown.css"
cp "$gmc/license" "$out/LICENSE-github-markdown-css"

hljs="$work/hljs/package"
cp "$hljs/styles/github.min.css" "$out/hljs-github.min.css"
cp "$hljs/styles/github-dark.min.css" "$out/hljs-github-dark.min.css"
cp "$hljs/LICENSE" "$out/LICENSE-highlight.js"
# The common bundle followed by every grammar, which register themselves with
# the hljs global; see assets/vendor/README.md.
{
  cat "$hljs/highlight.min.js"
  for f in $(cd "$hljs" && LC_ALL=C ls languages/*.min.js); do
    printf '\n'
    cat "$hljs/$f"
  done
  printf '\n'
} > "$out/highlight.all.min.js"

mermaid="$work/mermaid/package"
cp "$mermaid/dist/mermaid.min.js" "$out/mermaid.min.js"
cp "$mermaid/LICENSE" "$out/LICENSE-mermaid"

status=0
for f in "$out"/*; do
  name=$(basename "$f")
  if $check; then
    if ! cmp -s "$f" "$vendor/$name"; then
      echo "assets/vendor/$name does not match package-lock.json" >&2
      status=1
    fi
  else
    cp "$f" "$vendor/$name"
  fi
done

if $check && [ "$status" -ne 0 ]; then
  echo "Run packaging/vendor/update.sh and commit the result." >&2
fi
exit "$status"
