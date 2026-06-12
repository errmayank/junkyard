# Junkyard

Cross-platform system trash API.

## Usage

### Discard

Use `discard` to move a single path to the trash:

```rust
use junkyard::discard;

std::fs::write("file.txt", b"junk")?;

let item = discard("file.txt")?;

assert_eq!(item.original_name(), "file.txt");
assert!(!std::fs::exists("file.txt")?);
```

### Discard Multiple Paths

Use `discard_all` to move multiple paths to the trash:

```rust
use junkyard::discard_all;

std::fs::write("first.txt", b"first")?;
std::fs::create_dir("directory")?;
std::fs::write("directory/second.txt", b"second")?;

let items = discard_all(["first.txt", "directory"])?;

assert_eq!(items.len(), 2);
assert_eq!(items[0].original_name(), "first.txt");
assert_eq!(items[1].original_name(), "directory");
```

All paths are resolved before any item is moved to the trash. If resolution fails, no items are moved. Once trashing begins, paths are processed in input order. If a later operation fails, earlier items may already be in the trash.

Symbolic links are moved as links; their targets are left in place.

## Notes

- Linux: Follows the Freedesktop trash specification.
- macOS: Uses `NSFileManager`.
- Windows: Uses the Shell Recycle Bin APIs.

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version 2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>
<br>
<sup>
Any contribution intentionally submitted for inclusion in this repository by you shall be dual-licensed as above, without any additional terms or conditions.
</sup>
