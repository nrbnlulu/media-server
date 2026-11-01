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
