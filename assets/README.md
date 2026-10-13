# TermPlete media

| Asset | Purpose |
| --- | --- |
| [termplete-logo-v2.png](termplete-logo-v2.png) | Current logo: terminal graphic with the revised sans-serif wordmark |
| [termplete-logo-v2.prompt.txt](termplete-logo-v2.prompt.txt) | Recorded image-generation prompt; design provenance, not runtime instructions |
| [termplete-preview.gif](termplete-preview.gif) | Optimized animated preview embedded in the README |
| [termplete-preview.mp4](termplete-preview.mp4) | Higher-quality 45-second demo linked from the animated preview |
| [termplete-preview.jpg](termplete-preview.jpg) | Still frame at 42 seconds retained for static contexts |
| [termplete-logo.png](termplete-logo.png) | Earlier logo retained for design history |
| [termplete-logo.prompt.txt](termplete-logo.prompt.txt) | Earlier generation prompt |

The MP4 uses H.264 at 960×874 and 15 fps (CRF 28), with metadata removed and
the MP4 index at the beginning for streaming. This is about 55% smaller than
the previous repository copy. The 480×437 GIF runs at 8 fps with a 96-color
palette; it adds inline animation without replacing the clearer MP4. The JPEG
is a scaled frame from the same recording.

The README embeds the local GIF and links it to the MP4. For native player
controls, GitHub can host the MP4 attachment and render its returned URL inline;
the repo-local GIF works without an uploaded attachment.

The logo is used as supplied; the text in its prompt file does not describe
additional edits applied to the checked-in image. Project media is covered by
[the repository license](../LICENSE).
