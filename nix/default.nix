{ pkgs ? import <nixpkgs> {} }:

pkgs.stdenv.mkDerivation {
  pname = "onedrive-mount";
  version = "0.2.0";
  src = ./.;

  nativeBuildInputs = [ pkgs.autoPatchelfHook ];

  buildInputs = with pkgs; [
    stdenv.cc.cc.lib
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
    libGL
    libxkbcommon
  ];

  phases = [ "installPhase" ];
  installPhase = ''
    mkdir -p $out/bin $out/share/applications $out/share/icons/hicolor/scalable/apps
    install -m755 $src/bin/onedrive-mount  $out/bin/
    install -m755 $src/bin/onedrive-mountd $out/bin/
    install -m644 $src/share/applications/onedrive-mount.desktop  $out/share/applications/
    install -m644 $src/share/icons/hicolor/scalable/apps/onedrive-mount.svg $out/share/icons/hicolor/scalable/apps/
  '';
}
