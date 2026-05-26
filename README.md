# UNFINISHED

`e2` - Simple, extensible code editor.

![](assets/e2.png)

# Build

```console
cargo b --profile=release-fast

# If you don't need audio
cargo b --profile=release-fast --no-default-features --features=wayland,bundled

# If you don't need wayland
cargo b --profile=release-fast --no-default-features --features=bundled

# If you don't need audio, wayland and bundled sdl2:
cargo b --profile=release-fast --no-default-features
```
