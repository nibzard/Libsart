---
name: margin
description: Inspect Margin writing status and process queued passage edit requests with conflict checks and writer review.
---

Use `margin context --json` for writing state, viewer context, and pending requests.
Viewer context is the last published snapshot. Check `captured_ms` before using it.
Selection offsets count UTF-8 bytes in the active document identified by `digest`.
Previews can be truncated. Inspect the request for complete captured source.
A missing viewer means no desk is attached; do not infer a selection.
Use `margin requests list --json` for the complete request list.

1. Inspect the requested identifier with `margin requests inspect ID --json`.
2. Use the exact claim owner supplied by the Margin hook. If unavailable, choose a unique owner for this session.
3. Claim with `margin requests claim ID --owner OWNER --json`. Respect existing claims and cancelled requests.
4. Read `base.text`, `start`, and `end`. These offsets count UTF-8 bytes. Prepare only the replacement for that span.
5. Save the replacement in a temporary UTF-8 file outside the writing scope.
6. Submit with `margin requests submit ID --owner OWNER --input FILE --explanation TEXT --json`.
7. Report the returned state. `result_ready` means the viewer still needs to apply the result.

Do not write the requested document directly. Submission never approves changes.
For interrupted claims, inspect the work and ask the writer before releasing another session's claim.
Respect cancelled and undone work. Use `margin open PATH --line LINE` to show a passage in the desk.
After a checkpoint, use `margin diff --json` and `margin note --group DIGEST --text TEXT` to explain changes.
