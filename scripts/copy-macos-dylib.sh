#!/bin/sh
set -eu

homebrew_path=""
exec_path="target/debug/bundle/osx/Mousehop.app/Contents/MacOS/mousehop"

usage() {
    cat <<EOF
$0: Copy all Homebrew libraries into the macOS app bundle.
USAGE: $0 [-h] [-b homebrew_path] [exec_path]

OPTIONS:
  -h, --help    Show this help message and exit
  -b            Path to Homebrew installation (default: $homebrew_path)
  exec_path     Path to the main executable in the app bundle
                (default: get from `brew --prefix`)

When macOS apps are linked to dynamic libraries (.dylib files),
the fully qualified path to the library is embedded in the binary.
If the libraries come from Homebrew, that means that Homebrew must be present
and the libraries must be installed in the same location on the user's machine.

This script copies all of the Homebrew libraries that an executable links to into the app bundle
and tells all the binaries in the bundle to look for them there.
EOF
}

# Gather command-line arguments
while test $# -gt 0; do
    case "$1" in
        -h | --help ) usage; exit 0;;
        -b | --homebrew ) homebrew_path="$1"; shift 2;;
        * ) exec_path="$1"; shift;;
    esac
done

if [ -z "$homebrew_path" ]; then
    homebrew_path="$(brew --prefix)"
fi

# Path to the .app bundle
bundle_path=$(dirname "$(dirname "$(dirname "$exec_path")")")
# Path to the Frameworks directory
fwks_path="$bundle_path/Contents/Frameworks"
mkdir -p "$fwks_path"
# Path to bundled GTK/GSettings data
resources_path="$bundle_path/Contents/Resources"
share_path="$resources_path/share"

# Copy and fix references for a binary (executable or dylib)
#
# This function will:
# - Copy any referenced dylibs from /opt/homebrew to the Frameworks directory
# - Update the binary to reference the local copy instead
# - Add the Frameworks directory to the binary's RPATH
# - Recursively process the copied dylibs
fix_references() {
  local bin="$1"
  # Keep the original Homebrew location alongside the bundled copy. Some
  # formulae use @rpath for dependencies in the same lib directory, so the
  # bundled path alone is not enough to find their source files.
  local source_bin="${2:-$bin}"
  local source_dir
  source_dir=$(dirname "$source_bin")

  # Inspect every dependency. Absolute Homebrew paths can be copied directly;
  # @rpath dependencies are resolved beside the source library first, then via
  # Homebrew's linked lib directory. This keeps the recursive dependency walk
  # complete when a formula switches its install names to @rpath.
  libs=$(otool -L "$bin" | awk 'NR > 1 {print $1}')

  echo "$libs" | while IFS= read -r old_path; do
    if [ -z "$old_path" ]; then
      continue
    fi

    local source_path=""
    case "$old_path" in
      "$homebrew_path"/*)
        source_path="$old_path"
        ;;
      @rpath/*)
        local relative_path="${old_path#@rpath/}"
        if [ -e "$source_dir/$relative_path" ]; then
          source_path="$source_dir/$relative_path"
        elif [ -e "$homebrew_path/lib/$relative_path" ]; then
          source_path="$homebrew_path/lib/$relative_path"
        else
          continue
        fi
        ;;
      *)
        continue
        ;;
    esac

    local base_name="$(basename "$source_path")"
    local dest="$fwks_path/$base_name"

    if [ ! -e "$dest" ]; then
      echo "Copying $source_path -> $dest"
      cp -f "$source_path" "$dest"
      # Ensure the copied dylib is writable so that xattr -rd /path/to/Lan\ Mouse.app works.
      chmod 644 "$dest"

      echo "Updating $dest to have install_name of @rpath/$base_name..."
      install_name_tool -id "@rpath/$base_name" "$dest"

      # Recursively process this dylib
      fix_references "$dest" "$source_path"
    fi

    if [ "$old_path" != "@rpath/$base_name" ]; then
      echo "Updating $bin to reference @rpath/$base_name..."
      install_name_tool -change "$old_path" "@rpath/$base_name" "$bin"
    fi
  done
}

fix_references "$exec_path"

# Also inspect libraries already present from an earlier invocation. The
# recursion above intentionally skips an existing destination to avoid cycles;
# this pass makes reruns repair an incomplete bundle instead of preserving it.
for bundled_lib in "$fwks_path"/*.dylib; do
  if [ ! -e "$bundled_lib" ]; then
    continue
  fi
  source_lib="$homebrew_path/lib/$(basename "$bundled_lib")"
  if [ ! -e "$source_lib" ]; then
    source_lib="$bundled_lib"
  fi
  fix_references "$bundled_lib" "$source_lib"
done

copy_runtime_data() {
  mkdir -p "$share_path"

  if [ -d "$homebrew_path/share/glib-2.0/schemas" ]; then
    mkdir -p "$share_path/glib-2.0"
    rm -rf "$share_path/glib-2.0/schemas"
    cp -RL "$homebrew_path/share/glib-2.0/schemas" "$share_path/glib-2.0/schemas"
    if command -v glib-compile-schemas >/dev/null 2>&1; then
      glib-compile-schemas "$share_path/glib-2.0/schemas"
    elif [ -x "$homebrew_path/bin/glib-compile-schemas" ]; then
      "$homebrew_path/bin/glib-compile-schemas" "$share_path/glib-2.0/schemas"
    fi
  fi

  if [ -d "$homebrew_path/share/gtk-4.0" ]; then
    rm -rf "$share_path/gtk-4.0"
    cp -RL "$homebrew_path/share/gtk-4.0" "$share_path/gtk-4.0"
  fi

  if [ -d "$homebrew_path/share/icons/Adwaita" ]; then
    mkdir -p "$share_path/icons"
    rm -rf "$share_path/icons/Adwaita"
    cp -RL "$homebrew_path/share/icons/Adwaita" "$share_path/icons/Adwaita"
  fi
}

copy_runtime_data

# cargo-bundle preserves the source path under Contents/Resources and
# rewrites any `..` segment to `_up_`, so a resource at
# `../target/menubar-template.png` lands at
# `Resources/_up_/target/menubar-template.png`. NSBundle
# pathForResource: only searches the Resources root (not arbitrary
# subdirs), so flatten the file back to the root.
if [ -f "$resources_path/_up_/target/menubar-template.png" ]; then
  mv "$resources_path/_up_/target/menubar-template.png" "$resources_path/menubar-template.png"
  rmdir "$resources_path/_up_/target" "$resources_path/_up_" 2>/dev/null || true
fi

# Ensure the main executable has our Frameworks path in its RPATH
if ! otool -l "$exec_path" | grep -q "@executable_path/../Frameworks"; then
  echo "Adding RPATH to $exec_path"
  install_name_tool -add_rpath "@executable_path/../Frameworks" "$exec_path"
fi

# Se-sign the .app
codesign --force --deep --sign - "$bundle_path"

# Exercise dyld against the finished dependency closure. `--version` exits
# before the GUI or daemon starts, but a missing transitive dylib still makes
# this command fail and prevents a broken bundle from being released.
echo "Verifying bundled executable dependencies..."
"$exec_path" --version >/dev/null

echo "Done!"
