`mesa-26.2.2-zink-kgsl-surfaceless.patch` is AERA Browser's Mesa patch, from
[AERA-Plugins/browser](https://github.com/AERA-Plugins/browser)
(`source/patches/`). It lets Zink pick Turnip's KGSL device, which has no DRM
render node. Applied with `patch -p1` to Mesa 26.2.2.
