#!/usr/bin/env python3
"""Package native Solo binaries and the pinned model as a macOS app and DMG."""

import argparse
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def package(version: str, target: str, release: Path, output: Path):
    if sys.platform != "darwin":
        raise RuntimeError("macOS packaging requires a native macOS host")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Expected a plain release version")
    if target not in {"aarch64-apple-darwin", "x86_64-apple-darwin"}:
        raise ValueError("Unsupported macOS target")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="solo-macos-") as temporary:
        stage = Path(temporary)
        app = stage / "Solo.app"
        binaries = app / "Contents/MacOS"
        resources = app / "Contents/Resources"
        binaries.mkdir(parents=True)
        resources.mkdir()
        for name in ("solo", "solo-tray"):
            shutil.copy2(release / name, binaries / name)
        shutil.copytree(release / "models", binaries / "models")
        for source in release.glob("*.dylib"):
            shutil.copy2(source, binaries / source.name, follow_symlinks=True)
        # No build-machine or Homebrew library paths may escape in a download.
        for binary in [binaries / "solo", binaries / "solo-tray", *binaries.glob("*.dylib")]:
            if binary.suffix == ".dylib":
                run("install_name_tool", "-id", "@loader_path/" + binary.name, str(binary))
            for line in run("otool", "-L", str(binary)).splitlines()[1:]:
                dependency = line.strip().split(" ", 1)[0]
                if dependency.startswith(("/System/Library/", "/usr/lib/")):
                    continue
                local = binaries / Path(dependency).name
                if not local.is_file():
                    raise RuntimeError(f"Unbundled dependency in {binary.name}: {dependency}")
                run("install_name_tool", "-change", dependency, "@loader_path/" + local.name, str(binary))
            run("codesign", "--force", "--sign", "-", str(binary))
        info = {
            "CFBundleIdentifier": "community.solo.desktop",
            "CFBundleName": "Solo",
            "CFBundleDisplayName": "Solo",
            "CFBundleExecutable": "solo-tray",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSMinimumSystemVersion": "15.0",
            "NSHighResolutionCapable": True,
        }
        with (app / "Contents/Info.plist").open("wb") as file:
            plistlib.dump(info, file)
        shutil.copy2(ROOT / "LICENSE", resources / "LICENSE")
        (stage / "README.txt").write_text(
            "Solo Community macOS preview\n\n"
            "Drag Solo.app into Applications, then open Solo.\n"
            "This preview has an ad-hoc integrity signature, not an Apple Developer ID\n"
            "signature or notarization. macOS may require approval in Privacy & Security.\n"
            "Only approve a download you verified from the Solo GitHub release.\n\n"
            "For terminal and agent hooks, install the CLI after copying the app:\n"
            "  bash install-cli.sh\n"
            "Add $HOME/.local/bin to your shell PATH, then restart your assistant.\n"
            "The app includes the offline model. No library or credentials are bundled.\n",
            encoding="utf-8",
        )
        (stage / "install-cli.sh").write_text(
            '#!/usr/bin/env bash\nset -euo pipefail\n'
            'app="${1:-/Applications/Solo.app}"\n'
            'test -x "$app/Contents/MacOS/solo"\n'
            'test -x "$app/Contents/MacOS/solo-tray"\n'
            'mkdir -p "$HOME/.local/bin"\n'
            'for name in solo solo-tray; do\n'
            '  destination="$HOME/.local/bin/$name"\n'
            '  if [[ -e "$destination" && ! -L "$destination" ]]; then\n'
            '    echo "Refusing to replace existing file: $destination" >&2; exit 1\n'
            '  fi\n'
            '  if [[ -L "$destination" && "$(readlink "$destination")" != "$app/Contents/MacOS/$name" ]]; then\n'
            '    echo "Refusing to replace another CLI link: $destination" >&2; exit 1\n'
            '  fi\n'
            'done\n'
            'for name in solo solo-tray; do\n'
            '  destination="$HOME/.local/bin/$name"\n'
            '  ln -sfn "$app/Contents/MacOS/$name" "$destination"\n'
            'done\n'
            'echo "Installed Solo CLI links in $HOME/.local/bin. Restart your assistant."\n',
            encoding="utf-8",
        )
        (stage / "install-cli.sh").chmod(0o755)
        run("plutil", "-lint", str(app / "Contents/Info.plist"))
        run("codesign", "--force", "--deep", "--sign", "-", str(app))
        run("codesign", "--verify", "--deep", "--strict", str(app))
        # Execute the relocated payload, not the original Cargo output.
        for name in ("solo", "solo-tray"):
            observed = run(str(binaries / name), "--version")
            if f"{name} {version}+" not in observed:
                raise RuntimeError(f"Wrong packaged version: {observed}")
        stem = f"solo-{version}-{target}"
        archive = output / (stem + ".tar.gz")
        with tarfile.open(archive, "w:gz") as tar:
            for name in ("Solo.app", "README.txt", "install-cli.sh"):
                tar.add(stage / name, arcname=name)
        (stage / "Applications").symlink_to("/Applications", target_is_directory=True)
        run("hdiutil", "create", "-volname", "Solo", "-srcfolder", str(stage),
            "-format", "UDZO", "-ov", str(output / (stem + ".dmg")))
        print(archive)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("target")
    parser.add_argument("release_dir", type=Path)
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    package(args.version, args.target, args.release_dir.resolve(), args.output_dir.resolve())
