#!/usr/bin/env python3
"""Summarise `tatarbot::timing` log lines into latency numbers.

Run the bot with RUST_LOG=info,serenity=warn,tatarbot::timing=debug NO_COLOR=1 and pipe or
pass its log here:

    python3 scripts/timing_report.py bot.log

Command to first audio = Discord -> bot (interaction id timestamp vs local clock)
                       + bot handling until the track is queued (defer, join, resolve)
                       + queued -> playable (stream/object opened and parsed; the next 20 ms
                         mixer tick sends audio)
Each /play or /radio is paired with the next "first audio ready" in the same guild.
Standard library only.
"""

import re
import statistics
import sys
from collections import defaultdict

FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')


def parse(line):
    if "tatarbot::timing" not in line:
        return None
    message = line.split("tatarbot::timing:", 1)[1].strip()
    name = FIELD.split(message)[0].strip()
    fields = {k: v.strip('"') for k, v in FIELD.findall(message)}
    return name, fields


def summary(values):
    if not values:
        return "n=0"
    values = sorted(values)
    p95 = values[min(len(values) - 1, round(0.95 * (len(values) - 1)))]
    return (f"n={len(values):<3} median={statistics.median(values):7.1f}  "
            f"p95={p95:7.1f}  max={values[-1]:7.1f}")


def main(lines):
    pending = {}  # guild -> command fields awaiting first audio
    first_audio = defaultdict(list)  # kind -> [(total, parts)]
    joins = defaultdict(list)
    searches, autocomplete, autocomplete_net = [], [], []

    for line in lines:
        parsed = parse(line)
        if not parsed:
            continue
        name, f = parsed
        if name == "music command" and f.get("command") in ("play", "radio") and f.get("ok") == "true":
            pending[f["guild"]] = f
        elif name == "first audio ready" and f["guild"] in pending:
            cmd = pending.pop(f["guild"])
            net, handled, playable = (float(cmd["discord_to_bot_ms"]), float(cmd["handled_ms"]),
                                      float(f["queued_to_playable_ms"]))
            first_audio[f["kind"]].append((net + handled + playable, net, handled, playable))
        elif name == "voice joined":
            joins["first join" if f["first_join"] == "true" else "already joined"].append(
                float(f["join_ms"]))
        elif name == "library search":
            searches.append(float(f["search_ms"]))
        elif name == "autocomplete":
            autocomplete.append(float(f["replied_ms"]))
            autocomplete_net.append(float(f["discord_to_bot_ms"]))

    print("Command to first audio (ms)")
    for kind, rows in sorted(first_audio.items()):
        print(f"  {kind:8} total     {summary([r[0] for r in rows])}")
        print(f"  {'':8} discord   {summary([r[1] for r in rows])}")
        print(f"  {'':8} handling  {summary([r[2] for r in rows])}")
        print(f"  {'':8} playable  {summary([r[3] for r in rows])}")
    print("Voice join (ms)")
    for kind, values in sorted(joins.items()):
        print(f"  {kind:15} {summary(values)}")
    print("Library search, database round trip (ms)")
    print(f"  {'search':15} {summary(searches)}")
    print("Autocomplete (ms)")
    print(f"  {'bot reply':15} {summary(autocomplete)}")
    print(f"  {'discord->bot':15} {summary(autocomplete_net)}")


if __name__ == "__main__":
    with open(sys.argv[1], encoding="utf-8") if len(sys.argv) > 1 else sys.stdin as source:
        main(source)
