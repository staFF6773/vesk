{ lib
, stdenv
, fetchurl
, zstd
, patchelf
, alsa-lib
, at-spi2-atk
, at-spi2-core
, atk
, brotli
, dbus
, expat
, flac
, fontconfig
, freetype
, glib
, harfbuzz
, libdrm
, libffi
, libGL
, libgbm
, libjpeg8
, libopus
, libpulseaudio
, libwebp
, libx11
, libxcb
, libxcomposite
, libxdamage
, libxext
, libxfixes
, libxkbcommon
, libxrandr
, libxshmfence
, libxml2
, libxslt
, minizip
, nspr
, nss
, pipewire
, systemdLibs
, zlib
}:

let
  # Mismo commit y version de Chromium que el CEF 150.0.14 de nixpkgs, pero
  # compilado por Arch con ffmpeg_branding="Chrome" y proprietary_codecs=true.
  # Sin esto Discord no decodifica H.264/AAC: sus GIFs y videos son MP4.
  version = "150.0.14";

  srcCef = fetchurl {
    url = "https://archive.archlinux.org/packages/c/cef/cef-${version}-1-x86_64.pkg.tar.zst";
    hash = "sha256-fB6DXC8+wQQhTFWiEtPQb8LsKUB99hBRSrMphHBaH0o=";
  };

  # Arch usa openh264 2.7 (soname 8); nixpkgs trae 2.6 (soname 7).
  srcOpenh264 = fetchurl {
    url = "https://archive.archlinux.org/packages/o/openh264/openh264-2.7.0-1-x86_64.pkg.tar.zst";
    hash = "sha256-wGNaQBqtpLyNKBEkHGSe0RS4NBk/5RdZxDrxKpyc66w=";
  };

  # cef-dll-sys exige este archivo para aceptar CEF_PATH sin descargar nada.
  archiveJson = builtins.toJSON {
    name = "cef_binary_${version}+g7c1aa68+chromium-150.0.7871.129_linux64_minimal.tar.bz2";
    sha1 = "";
    type = "minimal";
  };

  runtimeLibs = [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    atk
    brotli
    dbus
    expat
    flac
    fontconfig
    freetype
    glib
    harfbuzz
    libdrm
    libffi
    libGL
    libgbm
    libjpeg8
    libopus
    libpulseaudio
    libwebp
    libx11
    libxcb
    libxcomposite
    libxdamage
    libxext
    libxfixes
    libxkbcommon
    libxrandr
    libxshmfence
    libxml2
    libxslt
    minizip
    nspr
    nss
    pipewire
    systemdLibs
    zlib
    zstd
    stdenv.cc.cc
  ];

  rpath = lib.makeLibraryPath runtimeLibs;
in
stdenv.mkDerivation {
  pname = "cef-codecs";
  inherit version;

  dontStrip = true;
  dontPatchELF = true;

  nativeBuildInputs = [ zstd patchelf ];

  unpackPhase = ''
    runHook preUnpack
    mkdir -p cef openh264
    tar --zstd -xf ${srcCef} -C cef
    tar --zstd -xf ${srcOpenh264} -C openh264
    runHook postUnpack
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p "$out"
    cp -a cef/usr/lib/cef/. "$out/"
    chmod -R u+w "$out"

    # El loader de CEF busca $ORIGIN antes que el RPATH de Nix.
    cp -a openh264/usr/lib/libopenh264.so* "$out/"

    printf '%s\n' '${archiveJson}' > "$out/archive.json"

    # chrome-sandbox no puede ser setuid dentro del store de Nix.
    chmod 0755 "$out/chrome-sandbox"
    patchelf \
      --set-interpreter "${stdenv.cc.bintools.dynamicLinker}" \
      --set-rpath "\$ORIGIN:${rpath}" \
      "$out/chrome-sandbox"

    patchelf --set-rpath "\$ORIGIN:${rpath}" "$out/libcef.so"
    for lib in libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 \
               libopenh264.so.8; do
      [ -f "$out/$lib" ] || continue
      patchelf --set-rpath "\$ORIGIN:${rpath}" "$out/$lib"
    done

    mkdir -p "$out/licenses"
    cp -a cef/usr/share/licenses/cef "$out/licenses/cef" 2>/dev/null || true
    cp -a openh264/usr/share/licenses/openh264 "$out/licenses/openh264" 2>/dev/null || true

    runHook postInstall
  '';

  meta = {
    description = "CEF 150 con codecs H.264/AAC para Vesk";
    homepage = "https://chromiumembedded.github.io/cef";
    license = with lib.licenses; [ bsd3 bsd2 ];
    platforms = [ "x86_64-linux" ];
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
