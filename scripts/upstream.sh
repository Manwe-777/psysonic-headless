#!/usr/bin/env bash
# What changed upstream in the code psysonic-headless follows (upstream/files.tsv) since it was last
# synced (upstream/sources.tsv).
#
#   scripts/upstream.sh status            changes per source, as Markdown; exits 10 when any
#   scripts/upstream.sh diff [name|file]  the upstream diff, for one source or one headless file
#   scripts/upstream.sh mark name [rev]   record a source as synced up to rev (default: its tip)
#
# Upstreams are kept as blobless bare clones in $UPSTREAM_CACHE (default ~/.cache/psysonic-headless).
set -euo pipefail

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
sources_file="$root/upstream/sources.tsv"
files_file="$root/upstream/files.tsv"
cache=${UPSTREAM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/psysonic-headless/upstream}
max_commits=40

die() { echo "upstream.sh: $*" >&2; exit 1; }
rows() { grep -v '^#' "$1" | awk -F'\t' 'NF >= 3'; }

# name -> "url branch synced"
source_row() {
  rows "$sources_file" | awk -F'\t' -v n="$1" '$1 == n { print $2, $3, $4; f = 1 } END { exit !f }' ||
    die "no source named '$1' in upstream/sources.tsv"
}
source_names() { rows "$sources_file" | cut -f1; }

# Clones or updates a source; prints its git dir.
fetch() {
  local name=$1 url branch synced dir
  read -r url branch synced < <(source_row "$name")
  dir="$cache/$name.git"
  if [ ! -d "$dir" ]; then
    mkdir -p "$cache"
    git clone -q --bare --filter=blob:none --no-tags --single-branch --branch "$branch" "$url" "$dir"
  else
    git -C "$dir" fetch -q --filter=blob:none --no-tags origin "+refs/heads/$branch:refs/heads/$branch"
  fi
  git -C "$dir" cat-file -e "$synced^{commit}" 2>/dev/null ||
    die "$name: synced commit $synced is not on $branch"
  echo "$dir"
}

# The upstream paths a source is followed by.
paths_of() { rows "$files_file" | awk -F'\t' -v n="$1" '$2 == n { print $3 }' | sort -u; }

# Headless files following an upstream path (an exact file, or anything inside a followed folder).
followers() {
  rows "$files_file" | awk -F'\t' -v n="$1" -v p="$2" \
    '$2 == n && (p == $3 || index(p, $3 "/") == 1) { print $1 }' | sort -u | paste -sd, - | sed 's/,/, /g'
}

web_url() { local u=${1%.git}; echo "${u/#git@github.com:/https://github.com/}"; }

status() {
  local behind=0 name url branch synced dir tip
  for name in $(source_names); do
    read -r url branch synced < <(source_row "$name")
    dir=$(fetch "$name")
    tip=$(git -C "$dir" rev-parse "refs/heads/$branch")
    mapfile -t paths < <(paths_of "$name")
    local count
    count=$(git -C "$dir" rev-list --count --no-merges "$synced..$tip" -- "${paths[@]}")
    if [ "$count" -eq 0 ]; then
      echo "## $name: up to date"
      echo
      echo "Synced with \`${synced:0:8}\`; nothing followed has changed on \`$branch\` (tip \`${tip:0:8}\`)."
      echo
      continue
    fi
    behind=1
    echo "## $name: $count commit(s) to look at"
    echo
    echo "\`$branch\` from \`${synced:0:8}\` to \`${tip:0:8}\`: $(web_url "$url")/compare/${synced:0:12}...${tip:0:12}"
    echo
    echo "| Change | Upstream file | Headless file |"
    echo "| --- | --- | --- |"
    git -C "$dir" diff --name-status -M "$synced" "$tip" -- "${paths[@]}" |
      while IFS=$'\t' read -r change from to; do
        local shown="\`$from\`"
        if [ -n "$to" ]; then shown="\`$from\` → \`$to\`"; fi
        echo "| ${change:0:1} | $shown | $(followers "$name" "$from") |"
      done
    echo
    git -C "$dir" log --no-merges --format='- `%h` %s (%an, %as)' -n "$max_commits" "$synced..$tip" -- "${paths[@]}"
    [ "$count" -gt "$max_commits" ] && echo "- … and $((count - max_commits)) more"
    echo
  done
  [ "$behind" -eq 0 ] || exit 10
}

diff_cmd() {
  local arg=${1:-} name url branch synced dir
  for name in $(source_names); do
    if [ -n "$arg" ] && [ "$arg" != "$name" ]; then
      # A headless file: only the upstream paths it follows.
      mapfile -t paths < <(rows "$files_file" | awk -F'\t' -v n="$name" -v f="$arg" '$2 == n && $1 == f { print $3 }')
      [ "${#paths[@]}" -gt 0 ] || continue
    else
      mapfile -t paths < <(paths_of "$name")
    fi
    read -r url branch synced < <(source_row "$name")
    dir=$(fetch "$name")
    git -C "$dir" diff -M --stat --patch "$synced" "refs/heads/$branch" -- "${paths[@]}"
  done
}

mark() {
  local name=${1:?usage: upstream.sh mark name [rev]} url branch synced dir rev
  read -r url branch synced < <(source_row "$name")
  dir=$(fetch "$name")
  rev=$(git -C "$dir" rev-parse -q --verify "${2:-refs/heads/$branch}^{commit}") ||
    die "$name: no commit '${2:-}' on $url"
  git -C "$dir" merge-base --is-ancestor "$rev" "refs/heads/$branch" || die "$rev is not on $branch"
  awk -F'\t' -v OFS='\t' -v n="$name" -v r="$rev" '!/^#/ && $1 == n { $4 = r } { print }' \
    "$sources_file" > "$sources_file.tmp" && mv "$sources_file.tmp" "$sources_file"
  echo "$name: synced up to ${rev:0:8} (was ${synced:0:8})"
}

case ${1:-status} in
  status) status ;;
  diff) diff_cmd "${2:-}" ;;
  mark) shift; mark "$@" ;;
  *) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
