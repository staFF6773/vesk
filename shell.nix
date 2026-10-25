{ pkgs ? import <nixpkgs> { } }:
let
  # CEF 150 es la version contra la que compila el fork de Sable. El binario
  # de nixpkgs es mas nuevo, asi que lo sobreescribimos con el que Sable usa
  # en produccion (mismo chromium y mismos hashes).
  cef = pkgs.cef-binary.override {
    version = "150.0.14";
    gitRevision = "7c1aa68";
    chromiumVersion = "150.0.7871.129";
    srcHashes = {
      x86_64-linux = "sha256-QO9hPkVcrNB6p8gfQl76qLb3frg/E8wo1HDuuk5h+Y8=";
    };
  };

  # Evita que el build script de cef-rs intente descargar CEF otra vez.
  fakeArchiveJson = pkgs.writeText "archive.json" (
    builtins.toJSON {
      name = cef.src.name;
      sha1 = "";
      type = "minimal";
    }
  );

  cefFlat = pkgs.symlinkJoin {
    name = "cef-${cef.version}-flat";
    paths = [
      "${cef}/${cef.buildType}"
      "${cef}/Resources"
    ];
    postBuild = ''
      ln -s ${cef}/libcef_dll "$out/"
      ln -s ${fakeArchiveJson} "$out/archive.json"
    '';
  };

  cefRuntimeLibs = with pkgs; [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    cairo
    cups
    dbus
    expat
    gdk-pixbuf
    glib
    gtk3
    libdrm
    libgbm
    libGL
    libpulseaudio
    libxkbcommon
    libx11
    libxcb
    libxcomposite
    libxdamage
    libxext
    libxfixes
    libxrandr
    libxshmfence
    nspr
    nss
    pango
    pipewire
    systemdLibs
    udev
  ];

  runtimeLibs = with pkgs; [
    gtk3
    webkitgtk_4_1
    libsoup_3
    glib
    glib-networking
    gdk-pixbuf
    cairo
    pango
    atk
    librsvg
    libayatana-appindicator
    sqlite
    openssl
    gst_all_1.gstreamer
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-good
    gst_all_1.gst-plugins-bad
    gst_all_1.gst-plugins-ugly
    gst_all_1.gst-libav
  ] ++ cefRuntimeLibs;
in
pkgs.mkShell {
  buildInputs = with pkgs; [ cargo rustc rustfmt clippy pkg-config ] ++ runtimeLibs;

  shellHook = ''
    export CEF_PATH="${cefFlat}"
    export LD_LIBRARY_PATH="${cefFlat}:${pkgs.lib.makeLibraryPath runtimeLibs}:''${LD_LIBRARY_PATH:-}"
    export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}"

    # GSettings: sin los esquemas en XDG_DATA_DIRS, GTK y WebKit no ven tema,
    # fuentes ni preferencias y sueltan "gsettings_schema_source_lookup:
    # assertion 'source != NULL' failed" (tauri-apps/tauri#14187 en NixOS).
    export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:''${XDG_DATA_DIRS:-/run/current-system/sw/share}"

    # Sistema de ventanas. Sin VESK_GDK_BACKEND se detecta la sesion: Wayland
    # nativo si WAYLAND_DISPLAY esta definido, X11/XWayland en caso contrario.
    # VESK_GDK_BACKEND=x11 fuerza XWayland; =wayland fuerza Wayland nativo.
    if [ -n "''${VESK_GDK_BACKEND:-}" ]; then
      export GDK_BACKEND="$VESK_GDK_BACKEND"
    fi

    # Solo afecta al backend wry (WebKitGTK); inerte con CEF.
    export WEBKIT_DISABLE_DMABUF_RENDERER=1

    # cef-dll-sys copia libcef.so y sus recursos a target/<perfil> conservando
    # los permisos de solo lectura del store de Nix. En el siguiente build no
    # puede sobrescribirlos. Los hacemos escribibles antes de invocar cargo.
    cargo() {
      for dir in target/debug target/release; do
        [ -d "$dir" ] || continue
        chmod -R u+w \
          "$dir"/lib*.so* \
          "$dir"/locales \
          "$dir"/*.pak \
          "$dir"/*.dat \
          "$dir"/*.bin \
          "$dir"/*.json \
          "$dir"/chrome-sandbox \
          2>/dev/null || true
      done
      command cargo "$@"
    }
  '';
}
