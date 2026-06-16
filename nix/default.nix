{ pkgs ? import <nixpkgs> {} }:

let
  runtimeLibs = with pkgs; [
    stdenv.cc.cc.lib
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
    libGL
    libxkbcommon
  ];

  runtimeLibPath = pkgs.lib.makeLibraryPath runtimeLibs;

  pkg = pkgs.stdenv.mkDerivation {
    pname = "onedrive-mount";
    version = "0.2.0";
    src = ./.;

    nativeBuildInputs = [ pkgs.autoPatchelfHook pkgs.makeWrapper ];
    buildInputs = runtimeLibs;

    phases = [ "installPhase" ];
    installPhase = ''
      mkdir -p $out/bin $out/share/applications $out/share/icons/hicolor/scalable/apps

      install -m755 $src/bin/onedrive-mountd $out/bin/onedrive-mountd
      install -m755 $src/bin/onedrive-mount  $out/bin/onedrive-mount
      install -m644 $src/share/applications/onedrive-mount.desktop $out/share/applications/
      install -m644 $src/share/icons/hicolor/scalable/apps/onedrive-mount.svg \
        $out/share/icons/hicolor/scalable/apps/

      # xkbcommon-dl dlopen()s libxkbcommon-x11.so by bare name at runtime,
      # bypassing rpath. wrapProgram moves $out/bin/onedrive-mount to
      # $out/bin/.onedrive-mount-wrapped and replaces it with a shell script
      # that sets LD_LIBRARY_PATH then exec's the real binary in $out.
      wrapProgram $out/bin/onedrive-mount \
        --prefix LD_LIBRARY_PATH : "${runtimeLibPath}"
    '';
  };
in pkg
