#!/usr/bin/env python3
"""Exercise the real DVR recorder and compare independently decoded MP4 segments.

Requires local GStreamer x264enc/x265enc, FFmpeg and the Rust build dependencies.
Outputs generated videos and JSON reports outside the repository. A nonzero exit
means a case is incompatible; the optional Rust probe itself only generates data.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
CASES = [
    "long20-h264", "long45-h264", "long45-h265",
    "closed-b-h264", "closed-b-h265", "open-h264", "open-h265",
    "high40-h264", "high80-h264", "high80-gop10-h264",
]


def decode(path):
    result = subprocess.run(
        ["ffmpeg", "-hide_banner", "-v", "warning", "-i", str(path),
         "-map", "0:v:0", "-fps_mode", "passthrough", "-f", "framemd5", "-"],
        capture_output=True, text=True, timeout=120, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"FFmpeg failed for {path}: {result.stderr}")
    hashes = [line.split(",")[-1].strip() for line in result.stdout.splitlines()
              if line and not line.startswith("#")]
    return hashes, result.stderr


def decode_continuously(directory, recording):
    """Preserve decoder references across file boundaries, as an RTP session can."""
    extension = next(directory.glob("source.*")).suffix[1:]
    format_name = "hevc" if extension == "h265" else extension
    joined = directory / f"joined.{extension}"
    with joined.open("wb") as output:
        for segment in recording["segments"]:
            subprocess.run(
                ["ffmpeg", "-hide_banner", "-v", "error", "-i",
                 str(directory / segment["file"]), "-map", "0:v:0", "-c:v", "copy",
                 "-bsf:v", f"{format_name}_mp4toannexb", "-f", format_name, "-"],
                stdout=output, stderr=subprocess.PIPE, check=True, timeout=30,
            )
    hashes, log = decode(joined)
    joined.unlink()
    return hashes, log


def check(case, output, generate):
    if generate:
        env = {**os.environ, "DVR_STRESS_CASE": case, "DVR_STRESS_DIR": str(output)}
        with (output / f"{case}.log").open("w") as log:
            subprocess.run(
                ["cargo", "test", "--locked", "-p", "media-server",
                 "camera_compatibility_stress_probe", "--", "--ignored", "--nocapture"],
                cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT,
                check=True, timeout=180,
            )
    directory = output / case
    recording = json.loads((directory / "results.json").read_text())
    original, source_log = decode(next(directory.glob("source.*")))
    if len(original) != recording["input_frames"]:
        raise RuntimeError(f"Source decode incomplete for {case}")
    combined = []
    decoded = {}
    for segment in recording["segments"]:
        name = segment["file"]
        hashes, log = decode(directory / name)
        combined.extend(hashes)
        decoded[name] = {"frames": len(hashes), "decoder_log": log}
    seek_errors = [
        {"elapsed_seconds": point["elapsed_seconds"], "rewind_seconds": int(delay), "error": error}
        for point in recording["checkpoints"]
        for delay, error in point["rewinds_seconds"].items() if error != "ok"
    ]
    timestamp_warnings = {name: value["decoder_log"] for name, value in decoded.items()
                          if "Non-monotonic DTS" in value["decoder_log"]}
    read_errors = [segment for segment in recording["segments"] if segment["read_error"]]
    report = {
        "case": case, "measured_mbps": recording["mbps"],
        "source_frames": len(original), "independent_segment_frames": len(combined),
        "decoded_hashes_match": original == combined,
        "matching_hashes_in_order": sum(a == b for a, b in zip(original, combined)),
        "recent_rewind_failures": seek_errors, "segment_read_failures": read_errors,
        "source_decoder_log": source_log, "segments": decoded,
        "presentation_timestamp_warnings": timestamp_warnings,
    }
    if case.startswith(("open-", "closed-b-")):
        continuous, log = decode_continuously(directory, recording)
        report["continuous_decode"] = {
            "frames": len(continuous), "hashes_match": original == continuous,
            "decoder_log": log,
        }
    report["compatible_in_test"] = not (seek_errors or read_errors or timestamp_warnings or original != combined)
    (directory / "decode-results.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"{case}: {'PASS' if report['compatible_in_test'] else 'FAIL'}; "
          f"{recording['mbps']:.2f} Mbps; decoded {len(combined)}/{len(original)} frames; "
          f"rewind failures={len(seek_errors)}; read failures={len(read_errors)}; "
          f"timestamp warnings={len(timestamp_warnings)}", flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("/tmp/dvr-camera-check"))
    parser.add_argument("--cases", nargs="+", choices=CASES, default=CASES)
    parser.add_argument("--decode-only", action="store_true", help="reuse existing recorder output")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    reports = [check(case, output, not args.decode_only) for case in args.cases]
    (output / "summary.json").write_text(json.dumps(reports, indent=2) + "\n")
    return 0 if all(report["compatible_in_test"] for report in reports) else 1


if __name__ == "__main__":
    sys.exit(main())
