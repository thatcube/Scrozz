#!/usr/bin/env bash
# Sourced by the bundle builder; does not access the keychain by itself.

scrozz_default_signing_identity() {
  local existing="$1" available="$2" candidates count prefix

  if [[ -n "$existing" ]]; then
    if ! printf '%s\n' "$available" |
      awk -F'"' -v wanted="$existing" '$2 == wanted { found=1 } END { exit !found }'; then
      echo "make-app-bundle: the installed signing identity is unavailable: $existing" >&2
      echo "make-app-bundle: refusing to change the app identity and invalidate privacy grants." >&2
      return 1
    fi
    printf '%s\n' "$existing"
    return 0
  fi

  # Prefer the distribution identity on a new install. Updates instead retain
  # the existing identity above, including an established development identity.
  for prefix in "Developer ID Application:" "Apple Development:"; do
    candidates="$(printf '%s\n' "$available" |
      awk -F'"' -v prefix="$prefix" 'index($2, prefix) == 1 { print $2 }' | sort -u)"
    [[ -n "$candidates" ]] || continue
    count="$(printf '%s\n' "$candidates" | wc -l | tr -d ' ')"
    if [[ "$count" != "1" ]]; then
      echo "make-app-bundle: multiple $prefix identities; set SCROZZ_SIGN_IDENTITY explicitly." >&2
      return 1
    fi
    printf '%s\n' "$candidates"
    return 0
  done

  printf '%s\n' "-"
}
