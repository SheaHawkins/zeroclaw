#!/usr/bin/env bash

set -euo pipefail
export LC_ALL=C

if (( $# != 2 )); then
  echo "usage: $0 <target-version> <current-srcinfo>" >&2
  exit 2
fi

target_version="$1"
srcinfo_file="$2"
version_pattern='^[0-9]+(\.[0-9]+)*$'

if [[ ! "$target_version" =~ $version_pattern ]]; then
  echo "::error::Target AUR version is not a numeric dotted version: ${target_version}" >&2
  exit 2
fi

if [[ ! -f "$srcinfo_file" ]]; then
  echo "::error::The cloned AUR package has no .SRCINFO file." >&2
  exit 2
fi

current_versions=()
while IFS= read -r version; do
  current_versions+=("$version")
done < <(
  awk -F '[[:space:]]*=[[:space:]]*' \
    '$1 ~ /^[[:space:]]*pkgver[[:space:]]*$/ { print $2 }' \
    "$srcinfo_file"
)

if (( ${#current_versions[@]} != 1 )); then
  echo "::error::Expected exactly one pkgver in the cloned AUR .SRCINFO; found ${#current_versions[@]}." >&2
  exit 2
fi

current_version="${current_versions[0]}"
if [[ ! "$current_version" =~ $version_pattern ]]; then
  echo "::error::Current AUR version is not a numeric dotted version: ${current_version}" >&2
  exit 2
fi

normalize_component() {
  local component="$1"
  component="${component#"${component%%[!0]*}"}"
  printf '%s' "${component:-0}"
}

compare_versions() {
  local left="$1"
  local right="$2"
  local index left_component right_component max_components
  local -a left_parts right_parts

  IFS='.' read -r -a left_parts <<< "$left"
  IFS='.' read -r -a right_parts <<< "$right"
  max_components=${#left_parts[@]}
  if (( ${#right_parts[@]} > max_components )); then
    max_components=${#right_parts[@]}
  fi

  for (( index = 0; index < max_components; index++ )); do
    left_component="$(normalize_component "${left_parts[index]:-0}")"
    right_component="$(normalize_component "${right_parts[index]:-0}")"

    if (( ${#left_component} < ${#right_component} )); then
      return 1
    fi
    if (( ${#left_component} > ${#right_component} )); then
      return 0
    fi
    if [[ "$left_component" < "$right_component" ]]; then
      return 1
    fi
    if [[ "$left_component" > "$right_component" ]]; then
      return 0
    fi
  done

  return 0
}

if ! compare_versions "$target_version" "$current_version"; then
  echo "::error::Refusing AUR downgrade: target ${target_version} is older than current ${current_version}." >&2
  exit 3
fi

echo "AUR version guard accepted target ${target_version} (current ${current_version})."
