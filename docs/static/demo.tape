# kura demo. ascii-gif prepends the window chrome, theme, and framing;
# this tape carries only the action.
Set Width 1200
Set Height 700

Type "# Archive a YouTube video into a self-contained vault" Sleep 600ms Enter
Type "kura archive dQw4w9WgXcQ -o vault" Sleep 800ms Enter
Sleep 9s

Type "# It is all on disk: JSON, localised media, an inert HTML page" Sleep 600ms Enter
Type "ls vault/youtube/video-dqw4w9wgxcq" Sleep 800ms Enter
Sleep 2.5s

Type "# Read the manifest back: depth, counts, range, size" Sleep 600ms Enter
Type "kura info vault/youtube/video-dqw4w9wgxcq" Sleep 800ms Enter
Sleep 4s
