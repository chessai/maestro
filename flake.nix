{
  description = "maestro — advisor-centric agent harness (dev env + packaged binaries)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));

      # Per-system crane build scaffolding. Reused by both `packages` and
      # `checks` so the CLI + daemon are built exactly once (shared
      # `cargoArtifacts`).
      craneFor = pkgs:
        let
          craneLib = crane.mkLib pkgs;

          # The workspace source, filtered to Cargo-relevant files so unrelated
          # edits (docs, .claude/, …) don't bust the build cache.
          src = craneLib.cleanCargoSource ./.;

          commonArgs = {
            inherit src;
            # The workspace root Cargo.toml is a virtual manifest (no
            # `[package]`), so crane can't infer these — set them explicitly to
            # silence its placeholder warning and name every derivation.
            pname = "maestro";
            version = "0.0.0";
            strictDeps = true;

            # Build-time tooling. `pkg-config` + a C toolchain are needed because
            # `rusqlite` is compiled with its `bundled` (vendored C) SQLite.
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ ];

            # Build the whole workspace so BOTH `maestro` (maestro-cli) and
            # `maestro-daemon` binaries are produced. crane already targets the
            # whole workspace; `--bins` builds every binary target across it.
            cargoExtraArgs = "--bins";

            # No network at build time.
            doCheck = false;
          };

          # Compile just the dependency graph once; reused for the workspace
          # build and every check to keep rebuilds fast.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # The installable package: the whole workspace, release profile. crane
          # installs every produced binary into `$out/bin`, so `maestro` and
          # `maestro-daemon` land there as siblings — which is exactly what
          # `resolve_daemon_bin` (crates/maestro-cli/src/daemon.rs) needs to
          # locate the daemon next to the CLI.
          maestro = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            doCheck = false;
            meta.mainProgram = "maestro";
          });
        in
        { inherit craneLib commonArgs cargoArtifacts maestro; };
    in
    {
      packages = forAll (pkgs:
        let c = craneFor pkgs; in
        {
          maestro = c.maestro;
          default = c.maestro;
        });

      apps = forAll (pkgs:
        let
          c = craneFor pkgs;
          maestroApp = {
            type = "app";
            program = "${c.maestro}/bin/maestro";
          };
        in
        {
          maestro = maestroApp;
          default = maestroApp;
        });

      checks = forAll (pkgs:
        let c = craneFor pkgs; in
        {
          # `nix flake check` builds the whole workspace (both binaries). This
          # reuses the packaged derivation, so it costs nothing beyond the build
          # itself. clippy/test/fmt aren't wired here to avoid failing CI on
          # pre-existing repo-wide lint/format drift unrelated to packaging;
          # those are covered by the devShell (`cargo clippy`, `cargo test`).
          maestro-build = c.maestro;
        });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          name = "maestro-dev";
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            pkg-config
            git               # worktree lifecycle (ADR-006)
            sqlite            # sqlite3 CLI for inspecting journal.db
          ] ++ lib.optionals stdenv.isLinux [
            bubblewrap        # bwrap — L1 containment probe target
          ];
          env.RUST_BACKTRACE = "1";
        };
      });

      # L2 devShell variants (ADR-004) land here later:
      #   devShells.<system>.codex-rust, codex-rust-net, …
    };
}
