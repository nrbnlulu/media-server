`dvr-fragmented.mp4` contains 12 seconds of 16×16 black H.264 video at
30 frames per second, with one-second fragments and no mfra index. It was
created with the same muxer used by the recorder:

```sh
gst-launch-1.0 -q videotestsrc pattern=black num-buffers=360 \
  ! video/x-raw,width=16,height=16,framerate=30/1 \
  ! x264enc tune=zerolatency key-int-max=30 \
  ! h264parse ! isofmp4mux fragment-duration=1000000000 \
  ! filesink location=dvr-fragmented.mp4
```
