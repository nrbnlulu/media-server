- [x] Consumers
    - [x] RTSP(h264/5) support with restarts
- [ ] Publishers
    - [x] raw rtp with NAT hole punch 
        - [x] H264
        - [x] H265
        
- [x] RTSP->webrtc proxy
    - [ ] make sure that if the camera is initially not working we can still open rtp / webrtc session.    
    - [ ] GOP caching for rtsp streams (https://github.com/bluenviron/mediamtx/pull/4189)
        
- [x] DVR streaming - live rewind
    - [ ] check h265 recordings maybe transcode to VP9 automatically?
    - [ ] camera off handeling
    - [ ] dump into mp4 files after X timeout.
- [ ] VOD - Long-term archived recordings (days/weeks/months)
