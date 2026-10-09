"""Assembles a release's assets into one directory, as CI publishes them.

    python scripts/release_assets.py --version 0.3.0 --windows win --android android --out dist

`win` holds what the Windows job produced: the NSIS installer with its `.sig`,
and ymsync.exe / ymsync-relay.exe. `android` holds the signed APK.

Writes:
  ym-sync_<v>_x64-setup.exe (+ .sig)   the installer the desktop updater fetches
  ymsync-<v>-android.apk               the phone app
  ymsync-<v>-windows-x64-tools.zip     the CLI player and the standalone relay
  latest.json                          what releases/latest/download/latest.json serves
  SHA256SUMS.txt                       sums of everything above, LF line endings
"""

import argparse
import datetime
import hashlib
import json
import pathlib
import shutil
import zipfile

REPO = "mshivarev/ym-sync"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--windows", required=True, type=pathlib.Path)
    parser.add_argument("--android", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--notes", type=pathlib.Path, help="release notes; their first line goes into latest.json")
    args = parser.parse_args()

    version = args.version
    tag = f"v{version}"
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    root = pathlib.Path(__file__).resolve().parent.parent

    installer = next(args.windows.rglob(f"ym-sync_{version}_x64-setup.exe"))
    signature = installer.with_name(installer.name + ".sig")
    if not signature.exists():
        raise SystemExit(f"{signature.name} is missing — was TAURI_SIGNING_PRIVATE_KEY set?")
    shutil.copy2(installer, out / installer.name)
    shutil.copy2(signature, out / signature.name)

    apk = next(args.android.rglob("*.apk"))
    shutil.copy2(apk, out / f"ymsync-{version}-android.apk")

    tools = out / f"ymsync-{version}-windows-x64-tools.zip"
    with zipfile.ZipFile(tools, "w", zipfile.ZIP_DEFLATED) as archive:
        for name in ["LICENSE", "README.md"]:
            archive.write(root / name, name)
        for exe in ["ymsync.exe", "ymsync-relay.exe"]:
            archive.write(next(args.windows.rglob(exe)), exe)

    notes = ""
    if args.notes and args.notes.exists():
        lines = [line.strip("-* \t") for line in args.notes.read_text(encoding="utf-8").splitlines()]
        notes = "; ".join(line for line in lines if line and not line.startswith("#"))

    now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
    latest = {
        "version": version,
        "notes": notes,
        "pub_date": now.isoformat().replace("+00:00", "Z"),
        "platforms": {
            "windows-x86_64": {
                "signature": signature.read_text().strip(),
                "url": f"https://github.com/{REPO}/releases/download/{tag}/{installer.name}",
            }
        },
    }
    (out / "latest.json").write_text(json.dumps(latest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    assets = sorted(p for p in out.iterdir() if p.is_file() and p.name != "SHA256SUMS.txt")
    sums = [f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}" for p in assets]
    # LF: `sha256sum -c` reads a \r as part of every file name.
    (out / "SHA256SUMS.txt").write_text("\n".join(sums) + "\n", encoding="utf-8", newline="\n")
    print("\n".join(sums))


if __name__ == "__main__":
    main()
