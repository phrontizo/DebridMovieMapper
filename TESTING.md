# Testing Guide

## Overview

The project includes comprehensive unit and integration tests covering all major functionality.

## Prerequisites

### Environment Variables

Tests require Real-Debrid and TMDB API credentials in `.env` file:

```bash
RD_API_TOKEN=your_real_debrid_api_token
TMDB_API_KEY=your_tmdb_api_key
```

**⚠️ SECURITY NOTE**: The `.env` file is in `.gitignore` and should NEVER be committed to git.

### Getting API Keys

1. **Real-Debrid API Token**:
   - Login to https://real-debrid.com
   - Go to https://real-debrid.com/apitoken
   - Generate a new API token

2. **TMDB API Key**:
   - Register at https://www.themoviedb.org
   - Go to Settings → API → Request API Key
   - Choose "Developer" and fill in the form

## Test Suite

### Unit Tests (No API credentials required)

Located in `src/` files under `#[cfg(test)]` modules.

```bash
# Run all unit tests
cargo test --lib

# Run specific test
cargo test --lib test_nfo_generation
```

**Coverage:**
- ✅ NFO file generation and XML formatting
- ✅ VFS update and conflict resolution
- ✅ Duplicate handling (prefers larger files)
- ✅ Torrent identification from filenames
- ✅ Title cleaning and normalization
- ✅ TMDB search scoring and selection
- ✅ Show vs Movie detection
- ✅ Short title edge cases
- ✅ Generic title filtering

**11 unit tests - all passing**

### Integration Tests (Require API credentials)

#### 1. `integration_test.rs` - Full System Integration
Tests the complete workflow from RD torrents to VFS with WebDAV.

```bash
cargo test --test integration_test
```

**Tests:**
- Fetches real torrents from your RD account
- Identifies media via TMDB
- Builds VFS structure
- Validates STRM files in VFS
- Checks NFO files
- Verifies folder structure (Movies/Shows)

**Time:** ~30-60 seconds (fetches 20 torrents)

#### 2. `test_strm_generation.rs` - STRM File Validation
Tests STRM-specific functionality.

```bash
cargo test --test test_strm_generation
```

**Tests:**
- `test_strm_file_generation`: Validates STRM files contain valid RD URLs
- `test_strm_filename_conversion`: Verifies `.mkv` → `.strm` conversion
- `test_nfo_generation_with_strm`: Checks NFO files generated alongside STRM

**Time:** ~30 seconds

#### 3. `video_player_simulation.rs` - WebDAV Reading
Simulates how Jellyfin/Plex would read files through WebDAV.

```bash
cargo test --test video_player_simulation
```

**Tests:**
- Opens STRM files through WebDAV
- Reads STRM content
- Validates RD download URLs
- Tests NFO file reading
- Verifies XML content

**Time:** ~30 seconds

#### 4. `repair_integration_test.rs` - Repair System
Tests automatic torrent repair functionality.

```bash
cargo test --test repair_integration_test
```

**Tests:**
- Health check for broken torrents
- Automatic repair triggering
- Link validation
- Broken torrent detection

**Time:** ~60 seconds

#### 5. `test_all_rd_torrents.rs` - Full Library Scan
Tests identification of ALL torrents in your RD account.

```bash
cargo test --test test_all_rd_torrents
```

**Output:**
- Lists every torrent
- Shows identification results
- Displays metadata (title, year, TMDB ID)
- Useful for debugging identification issues

**Time:** 2-10 minutes (depends on library size)

#### 6. `test_identification_stats.rs` - Statistics
Provides identification success rate statistics.

```bash
cargo test --test test_identification_stats
```

**Output:**
- Total torrents: X
- Identified: Y (Z%)
- Unidentified: N
- Lists all unidentified torrents with reasons

**Time:** 2-10 minutes

#### 7. `test_short_titles.rs` - Edge Cases
Tests specific edge cases for short/difficult titles.

```bash
cargo test --test test_short_titles
```

**Tests:**
- "Us" (2019)
- "Don" (2022)
- "Ran" (1985)
- "Amy" (2015)

**Time:** ~10 seconds

## Running All Tests

### Quick Test (Unit + Fast Integration)
```bash
cargo test --lib && \
cargo test --test integration_test && \
cargo test --test test_strm_generation && \
cargo test --test video_player_simulation
```

### Full Test Suite
```bash
cargo test
```

**Note:** This will run ALL integration tests including slow ones (test_all_rd_torrents, test_identification_stats). Could take 5-15 minutes.

## Test Coverage Summary

| Component | Unit Tests | Integration Tests |
|-----------|-----------|-------------------|
| **Identification** | ✅ 8 tests | ✅ 3 tests |
| **VFS** | ✅ 3 tests | ✅ 2 tests |
| **STRM Generation** | - | ✅ 3 tests |
| **WebDAV** | - | ✅ 2 tests |
| **Repair** | - | ✅ 3 tests |
| **Real-Debrid API** | - | ✅ All tests |
| **TMDB API** | - | ✅ All tests |

**Total: 11 unit tests + 13 integration tests = 24 tests**

## Continuous Integration

Integration tests can be run in CI with credentials stored as secrets:

```yaml
# .github/workflows/test.yml
env:
  RD_API_TOKEN: ${{ secrets.RD_API_TOKEN }}
  TMDB_API_KEY: ${{ secrets.TMDB_API_KEY }}

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Run tests
        run: cargo test
```

## Debugging Failed Tests

### Integration Test Failed

1. Check API credentials in `.env`
2. Verify RD account has downloaded torrents
3. Check RD API is not rate-limited
4. Look at test output for specific error

### Unit Test Failed

This indicates a code bug - unit tests don't depend on external APIs.

1. Review the test assertion
2. Check recent code changes
3. Run test with `--nocapture` for full output:
   ```bash
   cargo test --lib test_name -- --nocapture
   ```

### Identification Tests Failing

If `test_identification_stats` shows low success rate:

1. Check TMDB API is working
2. Review unidentified torrents list
3. Look for patterns (short titles, special characters, etc.)
4. Consider improving `identification.rs` logic

## Performance Testing

### Benchmark Identification Speed

```bash
time cargo test --test test_all_rd_torrents -- --nocapture
```

Typical results:
- 100 torrents: ~30-60 seconds
- 500 torrents: 2-5 minutes
- 1000+ torrents: 5-15 minutes

### Benchmark STRM Generation

```bash
time cargo test --test test_strm_generation -- --nocapture
```

Should complete in <30 seconds for 5 torrents.

## Test Data

### Sample Test Torrents

Integration tests use your actual RD library, but you can add specific test cases:

```rust
// In tests/test_edge_cases.rs
#[tokio::test]
async fn test_specific_problematic_torrent() {
    // Test a specific torrent that was causing issues
    let torrent_id = "YOUR_TORRENT_ID";
    // ...
}
```

### Mock Data

Unit tests use mock data and don't hit real APIs:

```rust
// Example from identification tests
let info = TorrentInfo {
    id: "test_id".to_string(),
    filename: "Inception.2010.1080p.mkv".to_string(),
    // ...
};
```

## Troubleshooting

### "RD_API_TOKEN must be set"

Create a `.env` file in the project root with your credentials.

### Rate Limiting

If tests fail with 429 errors:
- RD API has rate limits (~100 requests/minute)
- Wait a few minutes and retry
- Run fewer integration tests at once

### Network Issues

Integration tests require internet access to:
- Real-Debrid API (api.real-debrid.com)
- TMDB API (api.themoviedb.org)

Check your network connection and firewall settings.

### Slow Tests

Integration tests that scan your entire library can be slow:
- `test_all_rd_torrents`: Processes every torrent
- `test_identification_stats`: Same

Use `cargo test --test integration_test` for faster testing during development.

## Adding New Tests

### Unit Test Template

```rust
// In src/your_module.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_your_function() {
        let result = your_function(input);
        assert_eq!(result, expected);
    }
}
```

### Integration Test Template

```rust
// In tests/test_your_feature.rs
use debridmoviemapper::*;

#[tokio::test]
async fn test_your_integration() {
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set");

    // Your test code
    assert!(condition);
}
```

## Best Practices

1. **Run unit tests frequently** - They're fast and don't use API calls
2. **Run integration tests before commits** - Catch issues early
3. **Don't commit .env** - Already in .gitignore
4. **Use descriptive test names** - Helps with debugging
5. **Add tests for bug fixes** - Prevent regressions
6. **Keep integration tests focused** - Test one thing per test
7. **Mock external dependencies in unit tests** - Don't hit real APIs

## Test Maintenance

### When to Update Tests

- After changing VFS structure → Update `integration_test.rs`
- After changing identification logic → Update `identification` tests
- After API changes → Update relevant integration tests
- After fixing bugs → Add regression test

### Keeping Tests Fast

- Use `.take(N)` to limit torrents processed
- Run expensive tests manually, not in CI
- Consider parallelization for large test suites

## Security Notes

⚠️ **NEVER commit API credentials to git**

The `.env` file is ignored by git. To verify:

```bash
git log --all --full-history -- .env
# Should show no output
```

If you accidentally committed `.env`:

```bash
# Remove from git history
git filter-branch --force --index-filter \
  "git rm --cached --ignore-unmatch .env" \
  --prune-empty --tag-name-filter cat -- --all

# Force push (dangerous!)
git push origin --force --all
```

Better: Rotate your API keys immediately if leaked.
