{ pkgs }:

pkgs.stdenv.mkDerivation {
  name = "onedrive-mount";
  src = ./.;
  phases = [ "installPhase" ];
  installPhase = ''
    mkdir -p $out/bin $out/share/applications $out/share/icons/hicolor/scalable/apps
    cp $src/bin/onedrive-mount  $out/bin/ && chmod +x $out/bin/onedrive-mount
    cp $src/bin/onedrive-mountd $out/bin/ && chmod +x $out/bin/onedrive-mountd
    cp $src/share/applications/onedrive-mount.desktop $out/share/applications/
    cp $src/share/icons/hicolor/scalable/apps/onedrive-mount.svg $out/share/icons/hicolor/scalable/apps/
  '';
}
