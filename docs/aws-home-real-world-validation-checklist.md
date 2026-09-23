# Browser and lifecycle checklist

Use this with the automated driver in
scripts/aws-home-real-world-validation.sh. The driver uses a temporary
results directory and protected curl configuration files; do not replace it
with command-line passwords.

## Browser

1. Install the private test CA in the browser or operating-system trust store.
2. Open the test domain and confirm there is no TLS warning.
3. Enter the site credential when challenged.
4. Confirm the JPEG renders and the MP4 begins playback.
5. Seek to the beginning, middle, and near the end of the video.
6. Select a file, upload it, and confirm the displayed size and SHA-256.
7. Cancel an upload or navigate away, then confirm health and media still work.

## Outage and restart checks

- Start active media traffic, stop home, and confirm requests fail within ten
  seconds. Restart home and confirm health recovers within fifteen seconds.
- Restart gateway while home is running. Confirm home reconnects and health
  recovers within fifteen seconds.
- Restart Nginx. Confirm the WSS session reconnects and media/uploads work.

Record timings, restart counts, OOM state, and deviations in the dated result
document template. Destroy the disposable AWS resources and revoke credentials
before treating the extraction gate as passed.
