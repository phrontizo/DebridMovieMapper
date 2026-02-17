# Jellyfin STRM Setup and Troubleshooting

## Common Issues with STRM Files

Based on the errors in `output.log`, here are the issues and solutions:

### Issue 1: Probe Errors During Library Scan

**Error in logs:**
```
[ERR] MediaBrowser.Providers.Movies.MovieMetadataService: Error in Probe Provider
System.NullReferenceException: Object reference not set to an instance of an object.
   at MediaBrowser.MediaEncoding.Encoder.EncodingUtils.GetFileInputArgument(String path, String inputPrefix)
```

**Cause:** Jellyfin tries to probe STRM files (which are tiny text files) as if they were video files.

**Solution:** This is **expected behavior** and can be ignored. The probe errors during scan are cosmetic. Jellyfin will still:
- Read the STRM file correctly
- Get the Real-Debrid URL
- Play the video

**To reduce log noise:**
1. In Jellyfin Dashboard → Logs
2. Set log level to "Warning" or "Error" only (hide INFO messages)

### Issue 2: Playback Failures - Transcoding Attempted

**Error in logs:**
```
[ERR] Jellyfin.Api.Middleware.ExceptionMiddleware: Error processing request. URL GET /videos/.../live.m3u8
   at Jellyfin.Api.Controllers.DynamicHlsController.GetLiveHlsStream(...)
```

**Cause:** Jellyfin is trying to transcode the STRM file instead of direct playing the URL inside it.

**This is the REAL problem** - Jellyfin should direct play, not transcode.

## Solution: Force Direct Play for STRM Files

### Method 1: User Playback Settings (Per-User)

For each Jellyfin user that will watch content:

1. **User Menu** → **Settings** (gear icon)
2. **Playback** section
3. Configure these settings:

**Video Quality:**
- Max streaming bitrate: Set high (20 Mbps or more)
- Internet streaming bitrate: Match your connection

**Playback:**
- **Internet quality**: Set to highest quality
- **Enable hardware acceleration**: Optional (may help)

**Most Important:**
- **Prefer fMP4-HLS Media Container**: **OFF/Unchecked**
- **Allow video playback that requires conversion**: **ON/Checked**
- **Maximum allowed video transcoding resolution**: **No limit**

### Method 2: Server Playback Settings (Global)

1. **Dashboard** → **Server** → **Playback**

**Transcoding:**
- **Hardware acceleration**: None (unless you have GPU)
- **Enable VPP Tone mapping**: OFF
- **Enable Tone mapping**: OFF

**Streaming:**
- **Prefer fMP4-HLS Container**: **OFF**
- **Allow fallback to insecure connections**: ON (if using http://)

### Method 3: Force Direct Play in Client

When playing a video:

1. Click the **3-dot menu** on the video
2. Select **Playback Data**
3. Look at "Play Method":
   - ✅ **DirectPlay** - Good!
   - ❌ **Transcode** - Bad! Jellyfin is trying to transcode
   - ❌ **DirectStream** - May work, but not ideal

If it says "Transcode":

1. **Stop playback**
2. Click **3-dot menu** again
3. Select "**Versions**" or "**Quality**"
4. Choose "**Original**" or highest quality
5. Try playing again

### Method 4: Disable Transcoding Entirely (Nuclear Option)

If Jellyfin keeps trying to transcode:

1. **Dashboard** → **Server** → **Playback**
2. **Transcoding** section:
   - **Enable video transcoding**: **OFF**
   - **Allow encoding in HEVC format**: **OFF**
   - **Transcoding thread count**: **0**

⚠️ **Warning:** This disables transcoding for all content. Only works if your devices can direct play everything.

## Verifying STRM Files Work

### Test 1: Check STRM File Contents

Mount with rclone, then read a STRM file:

```bash
# Mount WebDAV
rclone mount debrid: /mnt/debrid --vfs-cache-mode off &

# Read a STRM file
cat "/mnt/debrid/Movies/Inception (2010) [tmdbid-27205]/Inception.2010.1080p.strm"
```

**Expected output:**
```
https://download41.real-debrid.com/d/ABC123XYZ.../Inception.2010.1080p.mkv
```

If you see a valid https:// URL, STRM files are working.

### Test 2: Test URL Directly

Copy the URL from the STRM file and test in a browser or video player:

```bash
# Using curl to check if URL is valid
curl -I "https://download41.real-debrid.com/d/ABC123XYZ.../"
```

Should return `200 OK` if RD link is valid.

### Test 3: VLC/mpv Direct Test

Test the STRM file directly with VLC or mpv:

```bash
# VLC
vlc "/mnt/debrid/Movies/Inception (2010) [tmdbid-27205]/Inception.2010.1080p.strm"

# mpv
mpv "/mnt/debrid/Movies/Inception (2010) [tmdbid-27205]/Inception.2010.1080p.strm"
```

If VLC/mpv can play it, the STRM file is correct.

## Jellyfin Client-Specific Issues

### Web Browser

**Problem:** Browser may not support direct play of some codecs.

**Solution:**
- Use Jellyfin Desktop app instead
- Or enable transcoding for web client only

### Android/iOS Apps

**Problem:** Apps may default to transcoding for compatibility.

**Solution:**
- In app settings → Playback
- Set "Video quality" to "Maximum"
- Enable "Direct Play"
- Disable "Auto" quality selection

### Roku/Android TV/Smart TV

**Problem:** Some clients can't direct play STRM files.

**Solution:**
- Update client to latest version
- Try different client (Jellyfin for Kodi, etc.)
- May need transcoding enabled for these devices

## Advanced: Jellyfin STRM Plugin

There's an unofficial Jellyfin plugin for better STRM support:

https://github.com/n0thhhing/jellyfin-plugin-streamfiles

This plugin improves STRM file handling, but it's not required for basic functionality.

## Why Probe Errors Are OK

The probe errors during library scan (`Error in Probe Provider`) are **normal and can be ignored** because:

1. Jellyfin tries to extract codec info from the STRM file
2. STRM file is just text (no video metadata)
3. Probe fails (expected)
4. Jellyfin **still reads the URL** from the STRM file
5. Playback works fine when configured properly

**These errors do not affect:**
- ✅ Library scanning (completes successfully)
- ✅ Metadata fetching (uses NFO files)
- ✅ Playback (uses URL from STRM)

**What DOES matter:**
- ❌ Playback errors (`live.m3u8` errors)
- ❌ Transcode errors
- ❌ Direct play not working

## Checklist for Working STRM Playback

- [ ] STRM files contain valid Real-Debrid URLs
- [ ] rclone mount is active and accessible
- [ ] Jellyfin library added and scanned
- [ ] User playback settings: High quality, direct play enabled
- [ ] Server playback settings: Transcoding optional/disabled
- [ ] Test playback shows "DirectPlay" in playback data
- [ ] No `live.m3u8` errors in Jellyfin logs

## Still Having Issues?

### Check Jellyfin Logs

```bash
# In Jellyfin container
docker logs jellyfin 2>&1 | grep -i "strm\|playback\|direct"
```

Look for:
- ✅ "Opening file for playback: ...strm"
- ✅ "Playback started"
- ❌ "Transcoding required"
- ❌ "Conversion required"

### Check Network Connectivity

Jellyfin server must be able to reach Real-Debrid:

```bash
# From Jellyfin container
curl -I https://download.real-debrid.com
```

Should return `200 OK`.

### Check STRM URL Expiration

Real-Debrid URLs expire after ~1 hour. If videos won't play after sitting idle:

1. Restart debridmoviemapper (regenerates STRM content)
2. Or refresh the library in Jellyfin

The WebDAV server regenerates URLs on-the-fly when STRM files are read, so this should be automatic.

### Enable Debug Logging

In Jellyfin:
1. Dashboard → Logs
2. Set log level to "Debug"
3. Try playing a video
4. Check logs for detailed playback info

## Expected Behavior Summary

| Stage | Expected | Logs |
|-------|----------|------|
| **Library Scan** | Completes successfully | Probe errors (ignore) |
| **Metadata** | Fetched from TMDB | NFO provides ID |
| **Opening File** | Reads STRM file | "Opening file: ...strm" |
| **Playback Start** | Direct plays RD URL | "DirectPlay" method |
| **During Playback** | Streams from RD | No transcoding |

**If you see transcoding or `live.m3u8` errors, the issue is Jellyfin configuration, not the STRM files themselves.**

## Quick Fix Command

Try this in Jellyfin's database to force direct play (⚠️ backup first):

```sql
-- In Jellyfin SQLite database
UPDATE UserProfiles SET MaxStreamingBitrate = 999999999;
UPDATE UserProfiles SET EnableVideoPlaybackTranscoding = 0;
```

Then restart Jellyfin.

## Contact & Support

If issues persist after following this guide:

1. Check Jellyfin version (10.11+ recommended)
2. Verify rclone mount is working
3. Test STRM files directly with VLC
4. Check Jellyfin forums for STRM-specific issues
5. Consider using Jellyfin for Kodi (better STRM support)
