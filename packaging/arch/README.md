# Arch Linux packaging

This `PKGBUILD` is not published to the AUR -- build and install it
locally instead:

```sh
cd packaging/arch
makepkg -si
```

When cutting a new release, bump `pkgver` (and reset `pkgrel=1`) to
match the new tag, then run `updpkgsums` to replace the placeholder
`SKIP` checksum with a real one before publishing anywhere.
