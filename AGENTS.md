# Media Server - Agent Documentation

## Overview

This is a **high-performance realtime media streaming server** written in Rust, optimized for live streaming and DVR (Digital Video Recording) with live-rewind capabilities. It acts as a proxy/gateway that ingests RTSP camera streams and distributes them to multiple consumers via modern protocols (WebRTC and a custom WSC-RTP protocol).

## Goals & Purpose

- **Ingest** video from RTSP sources (IP cameras, RTSP servers)
- **Publish** to multiple clients simultaneously via WebRTC/WSC-RTP (websocket controlled RTP)
- **DVR** record source streams for instant rewind.

## Architecture

```
RTSP Source (Camera) 
    |
    v
RtspClient (Source)
    |-- RtpPacketizer (transforms NAL units to RTP packets)
    |       |
    |       |-- WebRtcSession (Consumer - WebRTC)
    |       |-- WscRtpPublisher (Consumer - WSC-RTP)
    |
    |-- DvrRecorder (records to disk)
            |
            v
        fragmanted MP4 Files
            |
            v
        DvrPlayer (for dvr/seeking)
___
┌───────────────────────────┐                                            
│           source X        │                                            
└───────────────────────────┘     ┌─────────────┐    ┌──────────────┐    
│           ┌───────►DVR    │     │ live / dvr  │    │  consumer 2  │    
│           │               ├─────►   switch    ┼────►    wsc-rtp   │    
│           │               │     └─────────────┘    └──────────────┘    
│   RTSP────►               │                                            
│           │               │     ┌─────────────┐    ┌──────────────┐    
│           │               │     │ live / dvr  │    │  consumer 1  │    
│           └───────►LIVE   ├─────►   switch    ┼────►    webrtc    │    
└───────────────────────────┘     └─────────────┘    └──────────────┘    
```

## Agentic guidelines

- all plan files u write whould be stored under <project_root>/PLANS/<your_plan_name>.md

