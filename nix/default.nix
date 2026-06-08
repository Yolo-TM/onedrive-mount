{ pkgs }:

pkgs.stdenv.mkDerivation {
  name = "onedrive-mount";
  src = ./.;

  nativeBuildInputs = [ pkgs.autoPatchelfHook ];

  buildInputs = with pkgs; [
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
    cp $src/bin/onedrive-mount  $out/bin/
    cp $src/bin/onedrive-mountd $out/bin/
    cp $src/share/applications/onedrive-mount.desktop $out/share/applications/
    cp $src/share/icons/hicolor/scalable/apps/onedrive-mount.svg $out/share/icons/hicolor/scalable/apps/
  '';
}
