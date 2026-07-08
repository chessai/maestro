{
  description = "maestro — advisor-centric agent harness (dev env)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in
    {
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
