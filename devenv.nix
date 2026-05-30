{ pkgs, ... }:

{
  env = {
    GREET = "devenv";
    RUSTC_WRAPPER = "sccache";

    DEVELOPER_DIR = "/Applications/Xcode.app/Contents/Developer";
    SDKROOT = "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";
  };

  packages = with pkgs; [
    git
    cmake
    sccache
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
      "rust-src"
    ];
  };

  enterShell = ''
    export PATH=$(echo "$PATH" | tr ':' '\n' | grep -v xcbuild | paste -sd: -)
  '';

  tasks = {
    "openlogi:run" = {
      description = "List connected Logitech HID++ devices.";
      exec = "cargo run -p openlogi-cli -- list";
    };
    "openlogi:gui" = {
      description = "Run the desktop app.";
      exec = "cargo run -p openlogi-gui";
    };
    "openlogi:check" = {
      description = "Run fmt, clippy, and tests.";
      exec = ''
        set -e
        cargo fmt --all -- --check
        cargo clippy --workspace --all-targets -- -D warnings
        cargo test --workspace
      '';
    };
    "openlogi:assets" = {
      description = "Sync device assets.";
      exec = "cargo run -p openlogi-cli --release -- assets sync";
    };
    "openlogi:bundle" = {
      description = "Build OpenLogi.app.";
      exec = ''
        set -e
        if ! command -v cargo-bundle >/dev/null; then
          CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc cargo install cargo-bundle --locked
        fi
        bash scripts/macos-icns.sh
        if [ "''${OPENLOGI_BUNDLE_ASSETS:-0}" = "1" ]; then
          cargo run -p openlogi-cli --release -- assets sync
        else
          rm -rf crates/openlogi-gui/assets
          mkdir -p crates/openlogi-gui/assets
        fi
        cd crates/openlogi-gui
        cargo bundle --release
        echo
        echo "Bundle ready: target/release/bundle/osx/OpenLogi.app"
      '';
    };
    "openlogi:dmg" = {
      description = "Build a macOS DMG.";
      exec = "bash scripts/package-macos.sh";
    };
  };
}
