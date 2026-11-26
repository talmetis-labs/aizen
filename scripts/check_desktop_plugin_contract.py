"""Check the Desktop plugin release files that travel with the three repositories."""

from argparse import ArgumentParser
from pathlib import Path


def main() -> None:
    parser = ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--pilot", type=Path, required=True, help="Directory containing the Go package")
    parser.add_argument("--web", type=Path, required=True)
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    examples = root / "examples/desktop-plugins"
    validator = root / "src/plugin/desktop_release.rs"
    example = examples / "aizen-release.json"
    guide = examples / "SELLER.vi.md"
    files = (
        (validator, args.desktop / "src-tauri/src/desktop_release.rs"),
        (example, args.desktop / "examples/desktop-plugins/aizen-release.json"),
        (example, args.pilot / "testdata/desktop-release-v1.json"),
        (example, args.web / "public/downloads/aizen-release-v1.json"),
        (guide, args.web / "public/downloads/aizen-plugin-standard-v1.md"),
    )
    drift = [
        f"{copy} differs from {source}"
        for source, copy in files
        if not copy.is_file() or copy.read_bytes() != source.read_bytes()
    ]
    if drift:
        parser.exit(1, "\n".join(drift) + "\n")
    print("Desktop plugin validator, reference manifest and seller guide match.")


if __name__ == "__main__":
    main()
