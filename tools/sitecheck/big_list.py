# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Build a big site catalog for sitecheck from the sites people really visit.

Uses Chrome's CrUX top-origins list (github.com/zakird/crux-top-lists): every origin in the top
10,000, then a random (fixed-seed) pick from ranks 10k-50k up to --count. Adult and gambling sites
are left out with the Block List Project's lists and StevenBlack's adult hosts, plus name keywords;
so are throwaway domains (.xyz, .top...), which at these ranks are mostly gambling, piracy mirrors
and scams. Categories are rank buckets: top-1k, top-5k, top-10k, top-50k.

    python tools/sitecheck/big_list.py                       # -> tools/sitecheck/sites-big.json
    python tools/sitecheck/sitecheck.py --sites tools/sitecheck/sites-big.json --label big --gentle --compact
"""

import argparse
import gzip
import json
import random
import re
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
SOURCES = {
    "crux-global.csv.gz": "https://raw.githubusercontent.com/zakird/crux-top-lists/main/data/global/current.csv.gz",
    "adult-sinfonietta.hosts": "https://raw.githubusercontent.com/StevenBlack/hosts/master/extensions/porn/sinfonietta/hosts",
    "adult-tiuxo.hosts": "https://raw.githubusercontent.com/StevenBlack/hosts/master/extensions/porn/tiuxo/hosts",
    "adult-blp.txt": "https://blocklistproject.github.io/Lists/alt-version/porn-nl.txt",
    "gambling-blp.txt": "https://blocklistproject.github.io/Lists/alt-version/gambling-nl.txt",
}
KEYWORDS = re.compile(
    r"porn|xxx|sex|hentai|nude|nsfw|xvideo|xhamster|onlyfans|fap|milf|erotic|escort|camgirl|chaturbate|"
    r"stripchat|rule34|boob|tits|redtube|youjizz|brazzers|spankbang|eporner|javhd|r18|adult|babe|"
    r"livejasmin|bongacams|cam4|casino|slot|togel|judi|gacor|betting|\dbet|bet\d|poker|lottery|satta")
JUNK_TLDS = (".xyz", ".top", ".icu", ".site", ".space", ".click", ".lol", ".cfd", ".sbs", ".buzz",
             ".monster", ".bond", ".my.id", ".fun", ".vip", ".cyou", ".rest", ".beauty", ".hair")
BUCKETS = [("1000", "top-1k"), ("5000", "top-5k"), ("10000", "top-10k"), ("50000", "top-50k")]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--count", type=int, default=15000, help="sites in the catalog (default 15000)")
    parser.add_argument("--out", type=Path, default=HERE / "sites-big.json")
    parser.add_argument("--data", type=Path, default=HERE / "out" / "big-list-data",
                        help="where the downloaded lists are kept")
    args = parser.parse_args()

    args.data.mkdir(parents=True, exist_ok=True)
    for name, url in SOURCES.items():
        if not (args.data / name).exists():
            print("Downloading", url)
            request = urllib.request.Request(url, headers={"User-Agent": "stillwater-sitecheck"})
            with urllib.request.urlopen(request, timeout=120) as response:
                (args.data / name).write_bytes(response.read())

    blocked = set()
    for name in SOURCES:
        if name.endswith((".hosts", ".txt")):
            for line in (args.data / name).read_text("utf-8", errors="replace").splitlines():
                parts = line.split("#")[0].split()
                if parts:
                    blocked.add(parts[-1].lower().removeprefix("www."))

    def unwanted(host: str) -> bool:
        if KEYWORDS.search(host) or host.endswith(JUNK_TLDS):
            return True
        labels = host.split(".")
        return any(".".join(labels[i:]) in blocked for i in range(len(labels) - 1))

    with gzip.open(args.data / "crux-global.csv.gz", "rt", encoding="utf-8") as source:
        rows = [line.strip().split(",") for line in source][1:]
    by_rank: dict[str, list[str]] = {}
    left_out = 0
    for origin, rank in rows:
        if unwanted(origin.split("://", 1)[1].split(":")[0].lower()):
            left_out += 1
        else:
            by_rank.setdefault(rank, []).append(origin + "/")

    catalog: dict[str, list[str]] = {}
    room = args.count
    for rank, name in BUCKETS:
        origins = sorted(by_rank.get(rank, []))
        if len(origins) > room:
            origins = sorted(random.Random(args.count).sample(origins, room))
        if origins:
            catalog[name] = origins
            room -= len(origins)
    args.out.write_text(json.dumps(catalog, indent=1), "utf-8")
    print({name: len(urls) for name, urls in catalog.items()}, "->", args.out,
          f"({left_out} adult, gambling or junk origins left out)")


if __name__ == "__main__":
    main()
