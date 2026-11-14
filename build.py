"""Build ClaudeUsage.exe with PyInstaller.

    python build.py

Reads __version__ from claude_usage.pyw and writes the exe to
build/<version>/ClaudeUsage.exe (with that version in its file properties).
"""
import os
import re
import shutil
import subprocess
import sys

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "claude_usage.pyw")
BUILD = os.path.join(HERE, "build")
CLAY = (0xD9, 0x77, 0x57)


def read_version():
    m = re.search(r'^__version__\s*=\s*"([\d.]+)"', open(SRC, encoding="utf-8").read(), re.M)
    if not m:
        sys.exit("No __version__ found in claude_usage.pyw")
    return m.group(1)


def make_icon(path):
    """App icon: the orange rounded square with two dark blocks (same as the popup logo)."""
    s = 256
    im = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rounded_rectangle((8, 8, s - 9, s - 9), radius=52, fill=CLAY)
    b, g = 52, 16
    x0, y0 = (s - (2 * b + g)) / 2, (s - b) / 2
    for x in (x0, x0 + b + g):
        d.rounded_rectangle((x, y0, x + b, y0 + b), radius=6, fill=(44, 18, 8))
    im.save(path, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])


def write_version_file(path, version):
    nums = (version.split(".") + ["0", "0", "0", "0"])[:4]
    t = ", ".join(nums)
    with open(path, "w", encoding="utf-8") as f:
        f.write(f"""VSVersionInfo(
  ffi=FixedFileInfo(filevers=({t}), prodvers=({t})),
  kids=[
    StringFileInfo([StringTable('040904B0', [
      StringStruct('FileDescription', 'Claude Usage'),
      StringStruct('ProductName', 'Claude Usage'),
      StringStruct('FileVersion', '{version}'),
      StringStruct('ProductVersion', '{version}'),
      StringStruct('OriginalFilename', 'ClaudeUsage.exe')])]),
    VarFileInfo([VarStruct('Translation', [1033, 1200])])
  ]
)
""")


def main():
    version = read_version()
    out_dir = os.path.join(BUILD, version)
    work = os.path.join(BUILD, "_work")
    os.makedirs(work, exist_ok=True)
    icon = os.path.join(work, "claude_usage.ico")
    make_icon(icon)
    vfile = os.path.join(work, "version_info.txt")
    write_version_file(vfile, version)

    cmd = [sys.executable, "-m", "PyInstaller", "--noconfirm", "--clean", "--onefile", "--windowed",
           "--name", "ClaudeUsage", "--icon", icon, "--version-file", vfile,
           "--distpath", out_dir, "--workpath", work, "--specpath", work, SRC]
    print("Building version", version)
    subprocess.run(cmd, check=True)
    shutil.rmtree(work, ignore_errors=True)
    print("Done:", os.path.join(out_dir, "ClaudeUsage.exe"))


if __name__ == "__main__":
    main()
