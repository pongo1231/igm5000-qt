{
  description = "ISY IGM 5000 gaming-mouse configurator (Rust + cxx-qt + Qt6 Widgets)";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      qt = pkgs.qt6;
      # cxx-qt finds Qt only through qmake, and nixpkgs' qtbase setup hook exports
      # QMAKE=<qtbase>/bin/qmake, whose split prefix cannot see qttools' qmlcachegen
      # (nixpkgs#486645). Merging the modules into one prefix and pointing QMAKE at it fixes that.
      qtModules = with qt; [ qtbase qtwayland qttools ];
      qtEnv = qt.env "qt-custom-${qt.qtbase.version}" qtModules;
      # Materialises qt.env's variables (they live in qtWrapperArgs, which only a
      # wrapper script expands) and points QMAKE at the merged prefix.
      qtEnvSetup = ''
        makeWrapper "$(type -p sh)" "$TMPDIR/qt-env.sh" "''${qtWrapperArgs[@]}" --argv0 qt-env-hook
        sed '/qt-env-hook/d' -i "$TMPDIR/qt-env.sh"
        source "$TMPDIR/qt-env.sh"
        export QMAKE="${qtEnv}/bin/qmake"
        export RUSTFLAGS="-C link-arg=-fuse-ld=mold''${RUSTFLAGS:+ $RUSTFLAGS}"
      '';
      commonNative = with pkgs; [ qtEnv qt.wrapQtAppsHook makeWrapper mold pkg-config ];
      # The dev shell needs its own toolchain: `nix develop` + `cargo run` is how
      # this is meant to be run (packages.default uses rustPlatform's rustc).
      rustToolchain = with pkgs; [ cargo rustc clippy rustfmt ];
      commonBuild = [ qtEnv pkgs.kdePackages.breeze pkgs.kdePackages.plasma-integration ];
    in {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = commonNative ++ rustToolchain;
        buildInputs = commonBuild;
        QT_QPA_PLATFORM = "wayland;xcb";
        shellHook = qtEnvSetup;
      };
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "igm5000";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = commonNative;
        buildInputs = commonBuild;
        preBuild = qtEnvSetup;
        qtWrapperArgs = [ "--set-default" "QT_QPA_PLATFORM" "wayland;xcb" ];
        doCheck = true;
        meta.mainProgram = "igm5000";
      };

      # Importing this is all a host needs: the udev rules plus the configurator
      # itself (taken from this flake's own package output).
      nixosModules.default = { pkgs, ... }: {
        # The same rules the udev file ships: read from it so they cannot drift.
        services.udev.extraRules = builtins.readFile ./99-isymouse.rules;
        environment.systemPackages = [
          self.packages.${pkgs.stdenv.hostPlatform.system}.default
        ];
      };
    };
}
