#!/usr/bin/env python3
"""Independent cross-check of an auto_sync source -> destination Compare.

Standalone: uses only the Python standard library and does NOT touch auto_sync's
Rust code or HTTP API, so agreeing counts are a genuine second opinion rather
than the engine validating itself.

Replicates the engine's walk semantics (see should_visit_path / entries_match):
  - each entry (dir / file / symlink) counts as one; symlinks are NOT followed
  - directories named .auto_sync_trash / _tmp / _probe are pruned at any depth;
    a plain .auto_sync_probe file is skipped too
  - a file "matches" when size is equal AND mtime is within modify_window_secs
    (default 1s, checksum off); a symlink matches when its target is equal

Usage:
    scripts/verify_diff.py [SRC_ROOT] [DST_ROOT] [--window SECS] [--expect-entries N]
Defaults: SRC_ROOT=/zfs  DST_ROOT=/zfs_pool  --window 1
"""
import argparse
import os
import stat
import sys

INTERNAL = {".auto_sync_trash", ".auto_sync_tmp", ".auto_sync_probe"}


def scan(root):
    """rel_path -> (kind, size, mtime, linktarget); kind in dir/file/link."""
    out = {}
    stack = [root]
    rootlen = len(root.rstrip("/")) + 1
    while stack:
        d = stack.pop()
        try:
            it = os.scandir(d)
        except OSError as e:
            print(f"  WARN scandir {d}: {e}", file=sys.stderr)
            continue
        with it:
            for e in it:
                if e.name in INTERNAL:
                    continue  # prune internal dir/file at any depth
                try:
                    st = e.stat(follow_symlinks=False)
                except OSError as ex:
                    print(f"  WARN stat {e.path}: {ex}", file=sys.stderr)
                    continue
                rel = e.path[rootlen:]
                m = st.st_mode
                if stat.S_ISLNK(m):
                    try:
                        tgt = os.readlink(e.path)
                    except OSError:
                        tgt = ""
                    out[rel] = ("link", 0, st.st_mtime, tgt)
                elif stat.S_ISDIR(m):
                    out[rel] = ("dir", 0, st.st_mtime, "")
                    stack.append(e.path)
                else:
                    out[rel] = ("file", st.st_size, st.st_mtime, "")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("src_root", nargs="?", default="/zfs")
    ap.add_argument("dst_root", nargs="?", default="/zfs_pool")
    ap.add_argument("--window", type=float, default=1.0,
                    help="mtime match window in seconds (default 1)")
    ap.add_argument("--expect-entries", type=int, default=None,
                    help="assert the source entry count equals this")
    args = ap.parse_args()

    print(f"scanning source {args.src_root} ...", flush=True)
    src = scan(args.src_root)
    print(f"  source entries: {len(src)}", flush=True)
    print(f"scanning dest   {args.dst_root} ...", flush=True)
    dst = scan(args.dst_root)
    print(f"  dest entries:   {len(dst)}", flush=True)

    src_keys, dst_keys = set(src), set(dst)
    only_src = src_keys - dst_keys          # engine: to_add
    only_dst = dst_keys - src_keys          # engine: to_delete (mirror)
    common = src_keys & dst_keys

    type_mismatch, content_diff, matched = [], [], 0
    for k in common:
        sk, dk = src[k], dst[k]
        if sk[0] != dk[0]:
            type_mismatch.append(k)
            continue
        kind = sk[0]
        if kind == "dir":
            matched += 1
        elif kind == "link":
            if sk[3] == dk[3]:
                matched += 1
            else:
                content_diff.append(k)
        else:  # file
            if sk[1] == dk[1] and abs(sk[2] - dk[2]) <= args.window:
                matched += 1
            else:
                content_diff.append(k)

    by_kind = {}
    for k in src:
        by_kind[src[k][0]] = by_kind.get(src[k][0], 0) + 1

    total_diff = len(only_src) + len(only_dst) + len(type_mismatch) + len(content_diff)
    print("\n===== RESULT =====")
    print(f"source entries          : {len(src)}   (breakdown {by_kind})")
    print(f"dest entries            : {len(dst)}")
    print(f"matched (dir+file+link) : {matched}")
    print(f"to_add   (source-only)  : {len(only_src)}")
    print(f"to_delete(dest-only)    : {len(only_dst)}")
    print(f"to_update(size/mtime)   : {len(content_diff)}")
    print(f"type_mismatch           : {len(type_mismatch)}")
    print(f"TOTAL differences       : {total_diff}")
    if args.expect_entries is not None:
        print(f"\nentry-count match : {len(src) == args.expect_entries}")
    print(f"zero-diff match   : {total_diff == 0}")
    for label, lst in (("source-only", only_src), ("dest-only", only_dst),
                       ("type_mismatch", type_mismatch), ("content_diff", content_diff)):
        if lst:
            print(f"\nsample {label} (up to 15):")
            for p in sorted(lst)[:15]:
                print("  ", p)

    sys.exit(1 if total_diff else 0)


if __name__ == "__main__":
    main()
