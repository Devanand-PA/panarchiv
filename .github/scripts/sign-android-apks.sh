#!/usr/bin/env bash
set -euo pipefail

apk_dir="${1:?usage: sign-android-apks.sh <apk-output-directory>}"
keystore_path="${ANDROID_KEYSTORE_PATH:?ANDROID_KEYSTORE_PATH is required}"
keystore_password="${ANDROID_KEYSTORE_PASSWORD:?ANDROID_KEYSTORE_PASSWORD is required}"
key_alias="${ANDROID_KEY_ALIAS:?ANDROID_KEY_ALIAS is required}"
key_password="${ANDROID_KEY_PASSWORD:-$keystore_password}"

if [[ ! -d "$apk_dir" ]]; then
  echo "Android APK output directory does not exist: $apk_dir" >&2
  exit 1
fi

if [[ ! -f "$keystore_path" ]]; then
  echo "Android signing keystore does not exist: $keystore_path" >&2
  exit 1
fi

sdk_root="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"

find_sdk_tool() {
  local tool_name="$1"
  local override_path="$2"

  if [[ -n "$override_path" ]]; then
    printf '%s\n' "$override_path"
    return
  fi

  if command -v "$tool_name" >/dev/null 2>&1; then
    command -v "$tool_name"
    return
  fi

  if [[ -n "$sdk_root" && -d "$sdk_root/build-tools" ]]; then
    find "$sdk_root/build-tools" -maxdepth 2 -type f -name "$tool_name" -print | sort -V | tail -n 1
    return
  fi
}

apksigner_path="$(find_sdk_tool apksigner "${APKSIGNER_PATH:-}")"
zipalign_path="$(find_sdk_tool zipalign "${ZIPALIGN_PATH:-}")"

if [[ -z "$apksigner_path" || ! -x "$apksigner_path" ]]; then
  echo "Could not find an executable apksigner in Android SDK build-tools" >&2
  exit 1
fi

if [[ -z "$zipalign_path" || ! -x "$zipalign_path" ]]; then
  echo "Could not find an executable zipalign in Android SDK build-tools" >&2
  exit 1
fi

mapfile -d '' apks < <(find "$apk_dir" -type f -name '*.apk' -print0 | sort -z)
if ((${#apks[@]} == 0)); then
  echo "No APKs found under $apk_dir" >&2
  exit 1
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

for apk in "${apks[@]}"; do
  aligned_apk="$work_dir/$(basename "$apk").aligned"
  signed_apk="$work_dir/$(basename "$apk").signed"

  "$zipalign_path" -f -p 4 "$apk" "$aligned_apk"
  ANDROID_KEY_PASSWORD="$key_password" "$apksigner_path" sign \
    --ks "$keystore_path" \
    --ks-key-alias "$key_alias" \
    --ks-pass "env:ANDROID_KEYSTORE_PASSWORD" \
    --key-pass "env:ANDROID_KEY_PASSWORD" \
    --out "$signed_apk" \
    "$aligned_apk"
  "$apksigner_path" verify --verbose "$signed_apk"
  mv "$signed_apk" "$apk"
  echo "Signed $apk"
done
