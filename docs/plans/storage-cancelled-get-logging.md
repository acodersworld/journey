# Distinguish cancelled GETs from storage failures

## Summary

Add request context to storage-service error logs. Stop reporting a GET as a
failure only when h2 confirms the client cancelled its stream. Keep other
errors, including `unexpected frame type`, visible for investigation.

## Changes

- At the storage request boundary, retain the method, path, and HTTP/2 stream
  ID for error logging. Format the path safely and do not log headers or
  credentials.
- In the response sender, use h2 reset information to distinguish a peer
  `CANCEL` from an ordinary closed stream. Expose that distinction through
  `ServiceError`.
- Omit the failure log only for a confirmed peer cancellation of a GET. Log
  unclassified stream closures, PUT failures, and `unexpected frame type`
  with their request context. Do not infer cancellation from the error text
  alone.

## Verification

- Reset a large GET with `CANCEL` and verify it is classified as a client
  cancellation.
- Verify a non-cancellation send failure and a failed PUT remain visible with
  method, path, and stream ID.
- Reproduce video preview loading and seeking; confirm expected cancellations
  no longer flood stderr while unexplained errors retain enough context to
  diagnose.

## Assumption

A confirmed client-cancelled GET is routine when a browser abandons a video
range request. Other h2 errors remain actionable until their cause is
established.
