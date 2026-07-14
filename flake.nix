{
  description = "NASty - NAS System built on NixOS and bcachefs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    # ── bcachefs override (optional) ──────────────────────────────
    # Pinned to v1.38.8 release tag.
    # To revert to pure nixpkgs: comment out these two lines.
    # No other changes needed — bcachefs.nix defaults to pkgs.bcachefs-tools.
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.38.8";
    bcachefs-tools.inputs.nixpkgs.follows = "nixpkgs";

    # ── lanzaboote (Secure Boot for NixOS) ─────────────────────────
    # Pinned but inert by default. The lanzaboote module is loaded
    # into every NASty NixOS configuration so the `boot.lanzaboote.*`
    # option space exists, but `boot.lanzaboote.enable` stays false
    # unless the operator flips `nasty.secureBoot.enable = true`
    # (per-box opt-in, same shape as TPM2 binding). On boxes that
    # never opt in this is just a `flake.lock` entry and a few option
    # declarations — no boot path changes, no `lzbt` in the closure.
    #
    # Why pinned in nasty (not just in the wrapper): operators
    # shouldn't pick a lanzaboote rev — its protocol with sd-stub /
    # the firmware key formats / the install-hook contract are all
    # things NASty needs to test against, so we own the version.
    #
    # nixpkgs.follows: keeps lanzaboote's nixpkgs aligned with
    # nasty's, so cachix-substituted artifacts match by content
    # hash and we don't ship a second nixpkgs in the closure.
    lanzaboote.url = "github:nix-community/lanzaboote/v1.0.0";
    lanzaboote.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, bcachefs-tools, lanzaboote, ... }: let
    # Helper to build packages for a given system.
    #
    # The overlay wraps `fetchurl` so every curl invocation sends an
    # identifying User-Agent. crates.io enforces its long-standing
    # crawler policy (https://crates.io/policies) by rejecting
    # unidentifying UAs — `curl/X.Y.Z` is on the blocklist as of early
    # 2026, which makes `rustPlatform.importCargoLock` fail with HTTP
    # 403 for any crate tarball not already in the binary cache.
    # nixpkgs has a fix in flight (NixOS/nixpkgs#512735) but it will
    # take weeks to propagate; until then this overlay keeps every
    # PR that bumps a crate version unblocked.
    #
    # The UA matches the policy's recommended shape
    # (`appname (contact)`) so we don't need to dodge enforcement
    # again next time it tightens.
    mkPkgs = system: import nixpkgs {
      inherit system;
      overlays = [ (final: prev: {
        fetchurl = args:
          # Guard with isAttrs because some callers in nixpkgs (e.g.
          # makeOverridable wrappers / callPackage auto-call) pass a
          # function-shaped value here at evaluation time; only
          # actual fetch requests (attribute sets carrying url + hash)
          # get the User-Agent injection.
          if builtins.isAttrs args then
            prev.fetchurl (args // {
              curlOptsList = (args.curlOptsList or []) ++
                [ "-A" "nasty-engine-build (github.com/nasty-project/nasty)" ];
            })
          else
            prev.fetchurl args;
      }) ];
    };
    nasty-version = (builtins.fromTOML (builtins.readFile ./engine/Cargo.toml)).workspace.package.version;
    rootLock = builtins.fromJSON (builtins.readFile ./flake.lock);
    installerNastyOwner = "nasty-project";
    installerNastyRepo = "nasty";

    mkEngine = system: let
      pkgs = mkPkgs system;
      # The engine source plus the out-of-tree files it pulls in at
      # compile time:
      #   - `nixos/system-flake/flake.nix.template` and `flake.nix` are
      #     embedded via `include_str!` from `engine/nasty-system/src/update.rs`
      #     (wrapper-flake rendering + canonical bcachefs-tools ref).
      #   - `vendor/swagger-ui/` is embedded via `include_dir!` from
      #     `engine/nasty-engine/src/swagger_ui.rs` — the Swagger UI
      #     assets served at `/api/docs`.
      # Each path-up navigation walks out of `engine/` into the repo
      # root, so the Nix sandbox must contain these files at their
      # canonical relative positions for `cargo build` to compile.
      #
      # Why fileset.toSource (not just `src = ./.;`): the engine
      # source-hash only depends on engine sources + these few files.
      # Adding unrelated repo changes (docs, webui, nixos modules)
      # doesn't invalidate the Rust build cache.
      engineSrc = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          ./engine
          ./nixos/system-flake/flake.nix.template
          ./flake.nix
          ./vendor/swagger-ui
        ];
      };
    in pkgs.rustPlatform.buildRustPackage {
      pname = "nasty-engine";
      version = nasty-version;
      src = engineSrc;
      # `engineSrc` unpacks to `source/`; cargo runs from the engine
      # subdirectory so it finds the workspace Cargo.toml.
      sourceRoot = "source/engine";
      cargoLock.lockFile = ./engine/Cargo.lock;
      # webauthn-rs (added for #289 PR #1) pulls openssl-sys
      # transitively via webauthn-rs-core's COSE signature
      # verification path. The Nix sandbox doesn't expose system
      # headers, so the `openssl-sys` build script needs pkg-config
      # to locate the openssl libs the rest of the closure already
      # depends on. `nativeBuildInputs` is the right home for
      # pkg-config (it runs at build time on the host); `openssl`
      # itself goes in `buildInputs` so its dev headers land in the
      # CFLAGS / LD path the build script reads.
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.openssl ];
      # `rustPlatform.buildRustPackage` defaults to `doCheck = true`,
      # which runs `cargo test` in the test profile after the build
      # phase has already compiled everything in the release profile.
      # Different cargo profile → different cfg → different artifact
      # hashes → no sharing between phases → every crate gets compiled
      # twice (test-profile then release-profile). ~9 minutes per arch
      # on the CI runners.
      #
      # The test pass/fail signal is already covered by the standalone
      # `Engine (fmt, clippy, test)` CI job, which runs
      # `cargo test --workspace --all-targets` on a vanilla Ubuntu
      # runner. The Nix sandbox build phase still runs (this only
      # gates the check phase), so the sandbox-visibility gate — the
      # point of `nix-engine-build.yml`, catching `-sys` crates with
      # missing nix-side deps like openssl-sys → pkg-config + openssl
      # above — is unaffected. What's lost is narrowly "tests pass
      # even inside the hermetic sandbox", and the engine's test suite
      # isn't sandbox-fragile.
      doCheck = false;
      # Bake the flake's source rev into the engine binary as
      # NASTY_GIT_SHA, picked up by engine/nasty-system/build.rs and
      # exposed at runtime via option_env!. The engine uses this as
      # the authoritative answer to "what nasty rev am I" — see the
      # check() flow in update.rs. `self.rev` is set when the flake
      # tree is clean (committed); `self.dirtyRev` is set when
      # uncommitted changes are present (cargo / dev iteration); the
      # final "unknown" fallback is for flakes evaluated without git
      # at all (extremely unusual but possible).
      NASTY_GIT_SHA = self.rev or self.dirtyRev or "unknown";
      meta = {
        description = "NASty NAS engine";
        license = pkgs.lib.licenses.gpl3Only;
      };
    };

    mkWebui = system: let pkgs = mkPkgs system; in pkgs.buildNpmPackage {
      pname = "nasty-webui";
      version = nasty-version;
      src = ./webui;
      npmDepsHash = "sha256-ahCKFHNwmYYz0w7OPzkijkNoIgQpr5bZ+R6Wqg3G1n4=";
      npmFlags = [ "--legacy-peer-deps" ];
      buildPhase = ''
        npm run prepare
        npm run build
      '';
      installPhase = ''
        mkdir -p $out/share/nasty-webui
        cp -r build/* $out/share/nasty-webui/
      '';
    };

    mkBcachefsTools = system: let
      pkgs = mkPkgs system;
      # Override nixpkgs' bcachefs-tools with HEAD source from the flake input.
      # Using the nixpkgs package as the base preserves the `dkms` output and
      # `passthru.kernelModule` that the NixOS bcachefs module needs to build
      # the out-of-tree DKMS kernel module automatically via boot.bcachefs.package.
      # importCargoLock reads Cargo.lock directly — no pre-computed vendor hash needed.
      #
      # CONFIG_BCACHEFS_QUOTA: bcachefs is an out-of-tree DKMS module, so
      # its own Kconfig is never processed by the host kernel's build system.
      # We patch the DKMS Makefile to inject -DCONFIG_BCACHEFS_QUOTA directly,
      # enabling the VFS quotactl_ops (sb->s_qcop) that setquota/repquota need.
      base = pkgs.bcachefs-tools.overrideAttrs (old: {
        version = (builtins.fromTOML (builtins.readFile "${bcachefs-tools}/Cargo.toml")).package.version;
        src = bcachefs-tools;
        cargoDeps = pkgs.rustPlatform.importCargoLock {
          lockFile = "${bcachefs-tools}/Cargo.lock";
        };
        # bcachefs-tools v1.38.3 added libunwind as a pkg-config dep
        # (Makefile:113 fails with "pkg-config error: libunwind" without it).
        # Nixpkgs' base derivation hasn't picked this up yet, so we add it
        # to buildInputs here so the override builds against the new release.
        buildInputs = (old.buildInputs or []) ++ [ pkgs.libunwind ];
        # v1.38.8 drives the `bindgen` CLI from fs/build.rs (codegen.rs
        # execs $BINDGEN, default "bindgen") instead of using bindgen as
        # a build-dependency library. rust-bindgen is the wrapped binary
        # with its libclang plumbing included.
        nativeBuildInputs = (old.nativeBuildInputs or []) ++ [ pkgs.rust-bindgen ];
      });
    in base.overrideAttrs (old: {
      passthru = old.passthru // {
        # kernelModule must mirror nixpkgs' kernel-module.nix argument set so
        # `callPackage` fills them and we can forward them to the wrapped
        # derivation. nixpkgs added `rustPlatform` here (bcachefs's kernel
        # module is gaining Rust) — forward it too, or eval fails with
        # "called without required argument 'rustPlatform'". Keep this in
        # sync with pkgs/by-name/bc/bcachefs-tools/kernel-module.nix.
        kernelModule = { lib, stdenv, kernelModuleMakeFlags, kernel, rustPlatform }:
          (old.passthru.kernelModule {
            inherit lib stdenv kernelModuleMakeFlags kernel rustPlatform;
          }).overrideAttrs (kOld: {
            postPatch = (kOld.postPatch or "") + ''
              # Quota needs no patching since v1.38.8: fs/Makefile enables
              # CONFIG_BCACHEFS_QUOTA for DKMS builds whenever the host
              # kernel has CONFIG_QUOTA (NixOS kernels do), routed through
              # bcachefs-config-cppflags so the C compile and bindgen agree
              # on struct layout. Any future config injection here must use
              # that variable too — a bare `ccflags-y +=` diverges from
              # bindgen and corrupts memory silently (see fs/Makefile).
              # @NASTY_DEBUG_CHECKS_LINE@
            '';
          });
      };
    });

    mkNixosConfigs = system: let
      pkgs = mkPkgs system;
      nasty-engine = mkEngine system;
      nasty-webui = mkWebui system;
      nasty-bcachefs-tools = mkBcachefsTools system;
      installerNastyRef = "v${nasty-version}";
      installerSystemFlakeNix = builtins.replaceStrings
        [ "@NASTY_VERSION@" "@LOCAL_SYSTEM@" ]
        [ installerNastyRef system ]
        (builtins.readFile ./nixos/system-flake/flake.nix.template);
      # No seed flake.lock is bundled. PR #343 tried a minimal seed
      # (just nasty pre-resolved); install-time `nix flake lock` then
      # tripped on `nixpkgs follows non-existent input nasty/nixpkgs`
      # because nasty's own input subgraph wasn't in the wrapper's
      # lock either. Iterating the bundle further reintroduces the
      # follows-shape gymnastics PRs #340/#341 went through and the
      # class of bug they kept missing.
      #
      # iso.nix runs `nix flake lock` after bootstrap-system-flake
      # writes flake.nix and before nixos-install starts. Without a
      # seed lock to disagree with, nix builds the entire transitive
      # graph from the rendered template's input declarations
      # — fetching nasty + nixpkgs + bcachefs-tools from upstream.
      # That's ~20s of install-time network the operator's box can
      # afford (the install already needs network for binary cache
      # substitution).
      nastySystemFlakeSnapshot = pkgs.runCommand "nasty-system-flake-snapshot" {} ''
        mkdir -p "$out"
        cp ${self}/flake.nix "$out/flake.nix"
        cp ${self}/flake.lock "$out/flake.lock"
      '';
      installerSystemFlake = pkgs.runCommand "nasty-system-flake" {} ''
        mkdir -p "$out"
        cp ${./nixos/system-flake/hardware-configuration.nix} "$out/hardware-configuration.nix"
        cp ${./nixos/system-flake/networking.nix} "$out/networking.nix"
        cp ${pkgs.writeText "nasty-system-flake.nix" installerSystemFlakeNix} "$out/flake.nix"
      '';
    in rec {
      # Full NASty appliance configuration
      nasty = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot lanzaboote; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix

          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
        ];
      };

      nasty-rootfs = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot lanzaboote; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
          ({ lib, ... }: {
            boot.isContainer = true;
            # `boot.isContainer = true` flips
            # `networking.useHostResolvConf` to true by default (so
            # containers inherit the host's DNS), but nasty.nix also
            # enables systemd-resolved — and the two trip the
            # "Using host resolv.conf is not supported with
            # systemd-resolved" assertion.  This rootfs is bundled
            # into the ISO as a store path, so the failed assertion
            # blocks every ISO build.  Force-disable host resolv.conf
            # here; nothing actually consumes /etc/resolv.conf inside
            # this container payload (it's a pre-built closure, not
            # a running container).
            networking.useHostResolvConf = lib.mkForce false;
          })
        ];
      };

      # ISO image for installation
      nasty-iso = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = {
          inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nixpkgs;
          nasty-rootfs-toplevel = nasty-rootfs.config.system.build.toplevel;
          installerSystemFlake = installerSystemFlake;
          installerNastySource = self.outPath;
        };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./nixos/iso.nix
        ];
      };

      # Alternative ISO with systemd-boot for hardware where GRUB EFI fails
      # (e.g. ODROID H3 with JSL firmware)
      # Build: nix build .#nixosConfigurations.nasty-iso-sd.config.system.build.isoImage
      nasty-iso-sd = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = {
          inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nixpkgs;
          nasty-rootfs-toplevel = nasty-rootfs.config.system.build.toplevel;
          installerSystemFlake = installerSystemFlake;
          installerNastySource = self.outPath;
        };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./nixos/iso.nix
          ({ lib, ... }: {
            # Use systemd-boot instead of GRUB for EFI
            boot.loader.grub.enable = lib.mkForce false;
            boot.loader.systemd-boot.enable = lib.mkForce true;
          })
        ];
      };

      # QEMU VM for testing
      nasty-vm = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot lanzaboote; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix

          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
          ./nixos/vm.nix
        ];
      };

      # Cloud/CI disk image (Oracle Cloud compatible)
      nasty-cloud = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot lanzaboote; };
        modules = [
          "${nixpkgs}/nixos/modules/virtualisation/oci-image.nix"
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          ./nixos/modules/nasty.nix
          ./nixos/tls.nix
          ./nixos/cloud.nix
        ];
      };
    };

  in {
    # Export packages for both architectures
    packages.x86_64-linux = {
      engine = mkEngine "x86_64-linux";
      webui = mkWebui "x86_64-linux";
      bcachefs-tools = mkBcachefsTools "x86_64-linux";
      nasty-rootfs = (mkNixosConfigs "x86_64-linux").nasty-rootfs.config.system.build.toplevel;
      nasty-cloud-image = (mkNixosConfigs "x86_64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "x86_64-linux";
    };

    packages.aarch64-linux = {
      engine = mkEngine "aarch64-linux";
      webui = mkWebui "aarch64-linux";
      bcachefs-tools = mkBcachefsTools "aarch64-linux";
      nasty-rootfs = (mkNixosConfigs "aarch64-linux").nasty-rootfs.config.system.build.toplevel;
      nasty-cloud-image = (mkNixosConfigs "aarch64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "aarch64-linux";
    };

    # NixOS module
    nixosModules = {
      nasty = ./nixos/modules/nasty.nix;
      bcachefs = ./nixos/modules/bcachefs.nix;
      linuxquota = ./nixos/modules/linuxquota.nix;
      appliance-base = ./nixos/appliance-base.nix;
    };

    # NixOS configurations for both architectures
    nixosConfigurations = (mkNixosConfigs "x86_64-linux") // (
      let configs = mkNixosConfigs "aarch64-linux"; in {
        "nasty-aarch64" = configs.nasty;
        "nasty-rootfs-aarch64" = configs.nasty-rootfs;
        "nasty-iso-aarch64" = configs.nasty-iso;
        "nasty-vm-aarch64" = configs.nasty-vm;
        "nasty-cloud-aarch64" = configs.nasty-cloud;
      }
    );

    # Integration tests built via `nix build .#checks.x86_64-linux.<name>`.
    # Run by .github/workflows/integration.yml on push to main + manual dispatch.
    checks.x86_64-linux = let
      pkgs = mkPkgs "x86_64-linux";
      nasty-engine = mkEngine "x86_64-linux";
      nasty-webui = mkWebui "x86_64-linux";
      nasty-bcachefs-tools = mkBcachefsTools "x86_64-linux";
    in {
      bcachefs-smoke = import ./nixos/tests/bcachefs-smoke.nix {
        inherit pkgs nasty-bcachefs-tools;
      };
      appliance-smoke = import ./nixos/tests/appliance-smoke.nix {
        inherit pkgs nasty-engine nasty-webui nasty-bcachefs-tools;
      };
      ad-dc = import ./nixos/tests/ad-dc.nix {
        inherit pkgs nasty-engine nasty-webui nasty-bcachefs-tools;
      };
    };
  };
}
