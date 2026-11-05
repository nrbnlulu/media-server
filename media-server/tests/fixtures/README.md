`dvr-fragmented.mp4` contains 12 seconds of 16×16 black H.264 video at
30 frames per second, with one-second fragments and no mfra index. It was
generated with `isofmp4mux` as encoded input for the current recorder tests:

```sh
gst-launch-1.0 -q videotestsrc pattern=black num-buffers=360 \
  ! video/x-raw,width=16,height=16,framerate=30/1 \
  ! x264enc tune=zerolatency key-int-max=30 \
  ! h264parse ! isofmp4mux fragment-duration=1000000000 \
  ! filesink location=dvr-fragmented.mp4
```

`dvr-fragmented-h265.mp4` provides the same duration and frame rate with
64×64 H.265 video for segmented-recorder round-trip tests:

```sh
gst-launch-1.0 -q videotestsrc pattern=black num-buffers=360 \
  ! video/x-raw,width=64,height=64,framerate=30/1 \
  ! x265enc tune=zerolatency key-int-max=30 option-string=pools=1 \
  ! h265parse ! isofmp4mux fragment-duration=1000000000 \
  ! filesink location=dvr-fragmented-h265.mp4
```

`import-h264-no-sps-timing.mp4` is 626 frames of synthetic black H.264
video at 160×96 and 30 fps, with a keyframe interval of 325 frames and
millisecond MP4 timestamps. Its SPS omits VUI timing, matching the Milesight
export that exposed truncated segment tails. It contains no camera footage.
Generated with `videotestsrc pattern=black`, `x264enc tune=zerolatency
key-int-max=325`, and `mp4mux trak-timescale=1000`; the SPS timing flag and
its 65 timing bits were removed from the Annex-B export before remuxing
with `ffmpeg -r 30 -c copy -video_track_timescale 1000`.
