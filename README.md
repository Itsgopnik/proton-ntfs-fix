# proton-ntfs-fix

Small CLI tool that makes Steam libraries on NTFS partitions
compatible with Proton.

## The problem

Proton creates a `compatdata/<AppID>` folder (Wine prefix) for every
game that runs through it. That prefix relies on POSIX symlinks that
Proton itself creates when setting it up (e.g. `dosdevices/c:` ->
`../drive_c`). `ntfs3`/`ntfs-3g` don't reliably support such symlinks
-- if the Steam library lives on NTFS, this leads to broken prefixes
and games that fail to start.

The actual game files aren't affected by this and can stay on NTFS
without any issue -- only the `compatdata` folder needs to live on a
native Linux filesystem (ext4, btrfs, ...).

## What the tool does

1. Scans `<ntfs-library>/steamapps/compatdata/` for AppID folders.
2. Automatically filters out:
   - the folder `0` (shared/legacy prefix, not a real game),
   - AppIDs without a matching `appmanifest_<AppID>.acf` (typically
     runtime helper tools rather than real games).
3. For every remaining prefix that's still a real directory on NTFS
   (instead of a symlink):
   - recursively copies it into a temporary directory on the native
     filesystem (symlinks stay symlinks),
   - verifies the copy via an entry counter,
   - atomically renames it (now same filesystem) to the final name,
   - moves the NTFS original aside instead of deleting it right away,
   - creates a symlink to the native directory in its place,
   - only removes the moved-aside original once the symlink is in
     place.

If any step fails, the original data is preserved -- nothing gets
deleted before the symlink has been successfully created.

`fs::rename` doesn't work directly here because the NTFS library and
the native base directory live on different mounts (`EXDEV`) -- hence
the detour via copy + same-FS rename.

## Usage

```
proton-ntfs-fix [--dry-run] [--yes] [<AppID|all>] [ntfs-library] [native-base-dir]
```

| Argument | Meaning | Default |
|---|---|---|
| `<AppID\|all>` | a single AppID, or `all` for every AppID found | `all` |
| `ntfs-library` | path to the NTFS Steam library | `/mnt/ntfs/SteamLibrary` |
| `native-base-dir` | target directory for the prefixes | `$HOME/.local/share/steam-compatdata` |

Flags:

- `--dry-run` — only shows what would be done, doesn't change
  anything.
- `--yes`, `-y` — skips the confirmation prompt before moving.

### Examples

```sh
# Just see what would happen
proton-ntfs-fix --dry-run

# Move all affected prefixes (with confirmation)
proton-ntfs-fix

# Just one specific game, no prompt
proton-ntfs-fix --yes 570000

# Custom paths
proton-ntfs-fix all /mnt/games/SteamLibrary ~/steam-compatdata
```

## Requirements

- Steam should **not** be running (or at least the affected games
  shouldn't be active) while moving prefixes.
- Enough free space on the native target filesystem (prefixes are
  usually small, but disk usage briefly doubles for each one while
  it's being moved).

## Building

```sh
cargo build --release
```

The binary ends up at `target/release/proton-ntfs-fix`.

## License

[MIT](LICENSE)

---

*Not affiliated with or endorsed by Valve. Steam and Proton are
trademarks of Valve Corporation.*
