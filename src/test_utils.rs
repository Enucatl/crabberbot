// This file will contain shared helper functions for our tests.
use crate::downloader::MediaMetadata;

// This is the single, authoritative helper function.
pub fn create_test_metadata() -> MediaMetadata {
    MediaMetadata {
        filepath: "".to_string(),
        description: Some("".to_string()),
        title: Some("".to_string()),
        ext: "".to_string(),
        media_type: None,
        uploader: None,
        resolution: None,
        width: None,
        height: None,
        duration: None,
        filesize: None,
        entries: None,
        final_caption: "".to_string(),
    }
}
