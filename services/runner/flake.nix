{
  description = "runner development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.go_1_25
              pkgs.protobuf
              pkgs.buf
              pkgs.protoc-gen-go
              pkgs.protoc-gen-go-grpc
              # ARCHITECTURE: service sync uses this same dev shell when the
              # pinned proto container is unavailable. Keep every buf.gen
              # plugin here so the reproducible Nix fallback is complete.
              # grpc-gateway ships both protoc-gen-grpc-gateway and
              # protoc-gen-openapiv2 in nixpkgs.
              pkgs.grpc-gateway
              pkgs.protoc-gen-connect-go
              pkgs.grpcurl
              pkgs.golangci-lint
            ];
          };
        });
    };
}
