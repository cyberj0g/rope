#!/usr/bin/env python3

import gzip
import os
import sys
import tarfile
from pathlib import Path


def main() -> None:
    source = Path(sys.argv[1]).resolve()
    output = Path(sys.argv[2]).resolve()

    with output.open("wb") as destination:
        with gzip.GzipFile(fileobj=destination, mode="wb", mtime=0, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
                for path in sorted(source.rglob("*")):
                    relative = path.relative_to(source)
                    info = archive.gettarinfo(path, relative.as_posix())
                    info.uid = 0
                    info.gid = 0
                    info.uname = ""
                    info.gname = ""
                    info.mtime = 0
                    if path.is_file():
                        with path.open("rb") as contents:
                            archive.addfile(info, contents)
                    else:
                        archive.addfile(info)


if __name__ == "__main__":
    main()
