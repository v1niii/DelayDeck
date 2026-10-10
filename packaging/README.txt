DelayDeck: adjustable stream delay for OBS
==========================================

1. Run DelayDeck.exe.
   A small console window opens: that is DelayDeck running (close it to quit).
   The control panel opens in your browser at http://127.0.0.1:3000/

2. In OBS, go to Settings > Stream:
     Service:     Custom...
     Server:      rtmp://127.0.0.1:1935/live
     Stream Key:  your normal Twitch / YouTube / Kick stream key
   If you see "Enable Multitrack Video" (Twitch Enhanced Broadcasting),
   turn it off.

3. In the control panel's Settings tab, choose your platform.
   Choose "Test" to try everything without going live: nothing is sent
   anywhere, and "Watch output" on the Control tab shows the delayed stream.

4. Start streaming in OBS, then pick a delay in the panel.

Optional
  On-stream overlay: OBS > Sources > + > Browser
                     URL http://127.0.0.1:3000/overlay, width 1920, height 1080
  Panel inside OBS:  OBS > Docks > Custom Browser Docks
                     URL http://127.0.0.1:3000/

"Windows protected your PC"?
  DelayDeck is not code-signed, so Windows SmartScreen may warn the first
  time you run it. Click "More info", then "Run anyway".

Settings are saved in %APPDATA%\DelayDeck\config.json.
DelayDeck only listens on this PC (127.0.0.1) unless you change that there.
