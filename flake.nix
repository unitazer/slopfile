{
  description = "slopfile dev shell (rust + docker + nvim)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          rustfmt
          rust-analyzer
          clippy
          docker
          docker-compose
          neovim
          rustup
        ];
        shellHook = ''
          export DOCKER_CLI_PLUGINS_DIR="${pkgs.docker-compose}/libexec/docker/cli-plugins"
        '';
      };
    };
}
